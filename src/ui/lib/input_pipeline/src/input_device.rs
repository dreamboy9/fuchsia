// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{keyboard, media_buttons, mouse, touch},
    anyhow::{format_err, Error},
    async_trait::async_trait,
    async_utils::hanging_get::client::HangingGetStream,
    fdio,
    fidl::endpoints::Proxy,
    fidl_fuchsia_input_report as fidl_input_report,
    fidl_fuchsia_input_report::{InputDeviceMarker, InputReport},
    fuchsia_async as fasync, fuchsia_zircon as zx,
    futures::{channel::mpsc::Sender, stream::StreamExt},
    std::path::PathBuf,
};

/// The buffer size for the stream that InputEvents are sent over.
pub const INPUT_EVENT_BUFFER_SIZE: usize = 100;

/// The path to the input-report directory.
pub static INPUT_REPORT_PATH: &str = "/dev/class/input-report";

/// An `EventTime` indicates the time in nanoseconds when an event was first recorded.
pub type EventTime = u64;

/// An [`InputEvent`] holds information about an input event and the device that produced the event.
#[derive(Clone, Debug, PartialEq)]
pub struct InputEvent {
    /// The `device_event` contains the device-specific input event information.
    pub device_event: InputDeviceEvent,

    /// The `device_descriptor` contains static information about the device that generated the input event.
    pub device_descriptor: InputDeviceDescriptor,

    /// The time in nanoseconds when the event was first recorded.
    pub event_time: EventTime,
}

/// An [`InputDeviceEvent`] represents an input event from an input device.
///
/// [`InputDeviceEvent`]s contain more context than the raw [`InputReport`] they are parsed from.
/// For example, [`KeyboardEvent`] contains all the pressed keys, as well as the key's
/// phase (pressed, released, etc.).
///
/// Each [`InputDeviceBinding`] generates the type of [`InputDeviceEvent`]s that are appropriate
/// for their device.
#[derive(Clone, Debug, PartialEq)]
pub enum InputDeviceEvent {
    Keyboard(keyboard::KeyboardEvent),
    MediaButtons(media_buttons::MediaButtonsEvent),
    Mouse(mouse::MouseEvent),
    Touch(touch::TouchEvent),
}

/// An [`InputDescriptor`] describes the ranges of values a particular input device can generate.
///
/// For example, a [`InputDescriptor::Keyboard`] contains the keys available on the keyboard,
/// and a [`InputDescriptor::Touch`] contains the maximum number of touch contacts and the
/// range of x- and y-values each contact can take on.
///
/// The descriptor is sent alongside [`InputDeviceEvent`]s so clients can, for example, convert a
/// touch coordinate to a display coordinate. The descriptor is not expected to change for the
/// lifetime of a device binding.
#[derive(Clone, Debug, PartialEq)]
pub enum InputDeviceDescriptor {
    Keyboard(keyboard::KeyboardDeviceDescriptor),
    MediaButtons(media_buttons::MediaButtonsDeviceDescriptor),
    Mouse(mouse::MouseDeviceDescriptor),
    Touch(touch::TouchDeviceDescriptor),
}

#[derive(Clone, Copy, PartialEq)]
pub enum InputDeviceType {
    Keyboard,
    MediaButtons,
    Mouse,
    Touch,
}

/// An [`InputDeviceBinding`] represents a binding to an input device (e.g., a mouse).
///
/// [`InputDeviceBinding`]s expose information about the bound device. For example, a
/// [`MouseBinding`] exposes the ranges of possible x and y values the device can generate.
///
/// An [`InputPipeline`] manages [`InputDeviceBinding`]s and holds the receiving end of a channel
/// that an [`InputDeviceBinding`]s send [`InputEvent`]s over.
/// ```
#[async_trait]
pub trait InputDeviceBinding: Send {
    /// Returns information about the input device.
    fn get_device_descriptor(&self) -> InputDeviceDescriptor;

    /// Returns the input event stream's sender.
    fn input_event_sender(&self) -> Sender<InputEvent>;
}

/// Initializes the input report stream for the device bound to `device_proxy`.
///
/// Spawns a future which awaits input reports from the device and forwards them to
/// clients via `event_sender`.
///
/// # Parameters
/// - `device_proxy`: The device proxy which is used to get input reports.
/// - `device_descriptor`: The descriptor of the device bound to `device_proxy`.
/// - `event_sender`: The channel to send InputEvents to.
/// - `process_reports`: A function that generates InputEvent(s) from an InputReport and the
///                      InputReport that precedes it. Each type of input device defines how it
///                      processes InputReports.
pub fn initialize_report_stream<InputDeviceProcessReportsFn>(
    device_proxy: fidl_input_report::InputDeviceProxy,
    device_descriptor: InputDeviceDescriptor,
    mut event_sender: Sender<InputEvent>,
    mut process_reports: InputDeviceProcessReportsFn,
) where
    InputDeviceProcessReportsFn: 'static
        + Send
        + FnMut(
            InputReport,
            Option<InputReport>,
            &InputDeviceDescriptor,
            &mut Sender<InputEvent>,
        ) -> Option<InputReport>,
{
    fasync::Task::spawn(async move {
        let mut previous_report: Option<InputReport> = None;
        let (report_reader, server_end) = match fidl::endpoints::create_proxy() {
            Ok(res) => res,
            Err(_) => return, // TODO(fxbug.dev/54445): signal error
        };
        if device_proxy.get_input_reports_reader(server_end).is_err() {
            return; // TODO(fxbug.dev/54445): signal error
        }
        let mut report_stream =
            HangingGetStream::new(Box::new(move || Some(report_reader.read_input_reports())));
        loop {
            match report_stream.next().await {
                Some(Ok(Ok(input_reports))) => {
                    for report in input_reports {
                        previous_report = process_reports(
                            report,
                            previous_report,
                            &device_descriptor,
                            &mut event_sender,
                        );
                    }
                }
                Some(Ok(Err(_service_error))) => break,
                Some(Err(_fidl_error)) => break,
                None => break,
            }
        }
        // TODO(fxbug.dev/54445): Add signaling for when this loop exits, since it means the device
        // binding is no longer functional.
    })
    .detach();
}

/// Returns true if the device type of `input_device` matches `device_type`.
///
/// # Parameters
/// - `input_device`: The InputDevice to check the type of.
/// - `device_type`: The type of the device to compare to.
pub async fn is_device_type(
    input_device: &fidl_input_report::InputDeviceProxy,
    device_type: InputDeviceType,
) -> bool {
    let device_descriptor = match input_device.get_descriptor().await {
        Ok(descriptor) => descriptor,
        Err(_) => {
            return false;
        }
    };

    // Return if the device type matches the desired `device_type`.
    match device_type {
        InputDeviceType::MediaButtons => {
            let supported_buttons = media_buttons::MediaButtonsBinding::supported_buttons();
            if let Some(fidl_input_report::ConsumerControlDescriptor {
                input: Some(input), ..
            }) = device_descriptor.consumer_control
            {
                input
                    .buttons
                    .map(|buttons| buttons.iter().any(|button| supported_buttons.contains(button)))
                    .unwrap_or(false)
            } else {
                false
            }
        }
        InputDeviceType::Mouse => device_descriptor.mouse.is_some(),
        InputDeviceType::Touch => device_descriptor.touch.is_some(),
        InputDeviceType::Keyboard => device_descriptor.keyboard.is_some(),
    }
}

/// Returns a new [`InputDeviceBinding`] of the given device type.
///
/// # Parameters
/// - `device_type`: The type of the input device.
/// - `device_proxy`: The device proxy which is used to get input reports.
/// - `device_id`: The id of the connected input device.
/// - `input_event_sender`: The channel to send generated InputEvents to.
pub async fn get_device_binding(
    device_type: InputDeviceType,
    device_proxy: fidl_input_report::InputDeviceProxy,
    device_id: u32,
    input_event_sender: Sender<InputEvent>,
) -> Result<Box<dyn InputDeviceBinding>, Error> {
    match device_type {
        InputDeviceType::MediaButtons => Ok(Box::new(
            media_buttons::MediaButtonsBinding::new(device_proxy, input_event_sender).await?,
        )),
        InputDeviceType::Mouse => Ok(Box::new(
            mouse::MouseBinding::new(device_proxy, device_id, input_event_sender).await?,
        )),
        InputDeviceType::Touch => Ok(Box::new(
            touch::TouchBinding::new(device_proxy, device_id, input_event_sender).await?,
        )),
        InputDeviceType::Keyboard => {
            Ok(Box::new(keyboard::KeyboardBinding::new(device_proxy, input_event_sender).await?))
        }
    }
}

/// Returns a proxy to the InputDevice in `entry_path` if it exists.
///
/// # Parameters
/// - `dir_proxy`: The directory containing InputDevice connections.
/// - `entry_path`: The directory entry that contains an InputDevice.
///
/// # Errors
/// If there is an error connecting to the InputDevice in `entry_path`.
pub fn get_device_from_dir_entry_path(
    dir_proxy: &fidl_fuchsia_io::DirectoryProxy,
    entry_path: &PathBuf,
) -> Result<fidl_input_report::InputDeviceProxy, Error> {
    let input_device_path = entry_path.to_str();
    if input_device_path.is_none() {
        return Err(format_err!("Failed to get entry path as a string."));
    }

    let (input_device, server) = fidl::endpoints::create_proxy::<InputDeviceMarker>()?;
    fdio::service_connect_at(
        dir_proxy.as_channel().as_ref(),
        input_device_path.unwrap(),
        server.into_channel(),
    )
    .expect("Failed to connect to InputDevice.");
    Ok(input_device)
}

/// Returns the event time if it exists, otherwise returns the current time.
///
/// # Parameters
/// - `event_time`: The event time from an InputReport.
pub fn event_time_or_now(event_time: Option<i64>) -> EventTime {
    match event_time {
        Some(time) => time as EventTime,
        None => zx::Time::get_monotonic().into_nanos() as EventTime,
    }
}

#[cfg(test)]
mod tests {
    use {super::*, fidl::endpoints::spawn_stream_handler};

    #[test]
    fn max_event_time() {
        let event_time = event_time_or_now(Some(std::i64::MAX));
        assert_eq!(event_time, std::i64::MAX as EventTime);
    }

    #[test]
    fn min_event_time() {
        let event_time = event_time_or_now(Some(std::i64::MIN));
        assert_eq!(event_time, std::i64::MIN as EventTime);
    }

    // Tests that is_device_type() returns true for InputDeviceType::MediaButtons when a media
    // button device exists.
    #[fasync::run_singlethreaded(test)]
    async fn media_buttons_input_device_exists() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: None,
                        sensor: None,
                        touch: None,
                        keyboard: None,
                        consumer_control: Some(fidl_input_report::ConsumerControlDescriptor {
                            input: Some(fidl_input_report::ConsumerControlInputDescriptor {
                                buttons: Some(vec![
                                    fidl_input_report::ConsumerControlButton::VolumeUp,
                                    fidl_input_report::ConsumerControlButton::VolumeDown,
                                ]),
                                ..fidl_input_report::ConsumerControlInputDescriptor::EMPTY
                            }),
                            ..fidl_input_report::ConsumerControlDescriptor::EMPTY
                        }),
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(is_device_type(&input_device_proxy, InputDeviceType::MediaButtons).await);
    }

    // Tests that is_device_type() returns true for InputDeviceType::MediaButtons when a media
    // button device doesn't exist.
    #[fasync::run_singlethreaded(test)]
    async fn media_buttons_input_device_doesnt_exists() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: None,
                        sensor: None,
                        touch: None,
                        keyboard: None,
                        consumer_control: Some(fidl_input_report::ConsumerControlDescriptor {
                            input: Some(fidl_input_report::ConsumerControlInputDescriptor {
                                buttons: Some(vec![
                                    fidl_input_report::ConsumerControlButton::Reboot,
                                ]),
                                ..fidl_input_report::ConsumerControlInputDescriptor::EMPTY
                            }),
                            ..fidl_input_report::ConsumerControlDescriptor::EMPTY
                        }),
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(!is_device_type(&input_device_proxy, InputDeviceType::MediaButtons).await);
    }

    // Tests that is_device_type() returns true for InputDeviceType::Mouse when a mouse exists.
    #[fasync::run_singlethreaded(test)]
    async fn mouse_input_device_exists() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: Some(fidl_input_report::MouseDescriptor {
                            input: Some(fidl_input_report::MouseInputDescriptor {
                                movement_x: None,
                                movement_y: None,
                                position_x: None,
                                position_y: None,
                                scroll_v: None,
                                scroll_h: None,
                                buttons: None,
                                ..fidl_input_report::MouseInputDescriptor::EMPTY
                            }),
                            ..fidl_input_report::MouseDescriptor::EMPTY
                        }),
                        sensor: None,
                        touch: None,
                        keyboard: None,
                        consumer_control: None,
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(is_device_type(&input_device_proxy, InputDeviceType::Mouse).await);
    }

    // Tests that is_device_type() returns true for InputDeviceType::Mouse when a mouse doesn't
    // exist.
    #[fasync::run_singlethreaded(test)]
    async fn mouse_input_device_doesnt_exist() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: None,
                        sensor: None,
                        touch: None,
                        keyboard: None,
                        consumer_control: None,
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(!is_device_type(&input_device_proxy, InputDeviceType::Mouse).await);
    }

    // Tests that is_device_type() returns true for InputDeviceType::Touch when a touchscreen
    // exists.
    #[fasync::run_singlethreaded(test)]
    async fn touch_input_device_exists() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: None,
                        sensor: None,
                        touch: Some(fidl_input_report::TouchDescriptor {
                            input: Some(fidl_input_report::TouchInputDescriptor {
                                contacts: None,
                                max_contacts: None,
                                touch_type: None,
                                buttons: None,
                                ..fidl_input_report::TouchInputDescriptor::EMPTY
                            }),
                            ..fidl_input_report::TouchDescriptor::EMPTY
                        }),
                        keyboard: None,
                        consumer_control: None,
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(is_device_type(&input_device_proxy, InputDeviceType::Touch).await);
    }

    // Tests that is_device_type() returns true for InputDeviceType::Touch when a touchscreen
    // exists.
    #[fasync::run_singlethreaded(test)]
    async fn touch_input_device_doesnt_exist() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: None,
                        sensor: None,
                        touch: None,
                        keyboard: None,
                        consumer_control: None,
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(!is_device_type(&input_device_proxy, InputDeviceType::Touch).await);
    }

    // Tests that is_device_type() returns true for InputDeviceType::Keyboard when a keyboard
    // exists.
    #[fasync::run_singlethreaded(test)]
    async fn keyboard_input_device_exists() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: None,
                        sensor: None,
                        touch: None,
                        keyboard: Some(fidl_input_report::KeyboardDescriptor {
                            input: Some(fidl_input_report::KeyboardInputDescriptor {
                                keys3: None,
                                ..fidl_input_report::KeyboardInputDescriptor::EMPTY
                            }),
                            output: None,
                            ..fidl_input_report::KeyboardDescriptor::EMPTY
                        }),
                        consumer_control: None,
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(is_device_type(&input_device_proxy, InputDeviceType::Keyboard).await);
    }

    // Tests that is_device_type() returns true for InputDeviceType::Keyboard when a keyboard
    // exists.
    #[fasync::run_singlethreaded(test)]
    async fn keyboard_input_device_doesnt_exist() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: None,
                        sensor: None,
                        touch: None,
                        keyboard: None,
                        consumer_control: None,
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(!is_device_type(&input_device_proxy, InputDeviceType::Keyboard).await);
    }

    // Tests that is_device_type() returns true for every input device type that exists.
    #[fasync::run_singlethreaded(test)]
    async fn no_input_device_match() {
        let input_device_proxy = spawn_stream_handler(move |input_device_request| async move {
            match input_device_request {
                fidl_input_report::InputDeviceRequest::GetDescriptor { responder } => {
                    let _ = responder.send(fidl_input_report::DeviceDescriptor {
                        device_info: None,
                        mouse: Some(fidl_input_report::MouseDescriptor {
                            input: Some(fidl_input_report::MouseInputDescriptor {
                                movement_x: None,
                                movement_y: None,
                                position_x: None,
                                position_y: None,
                                scroll_v: None,
                                scroll_h: None,
                                buttons: None,
                                ..fidl_input_report::MouseInputDescriptor::EMPTY
                            }),
                            ..fidl_input_report::MouseDescriptor::EMPTY
                        }),
                        sensor: None,
                        touch: Some(fidl_input_report::TouchDescriptor {
                            input: Some(fidl_input_report::TouchInputDescriptor {
                                contacts: None,
                                max_contacts: None,
                                touch_type: None,
                                buttons: None,
                                ..fidl_input_report::TouchInputDescriptor::EMPTY
                            }),
                            ..fidl_input_report::TouchDescriptor::EMPTY
                        }),
                        keyboard: Some(fidl_input_report::KeyboardDescriptor {
                            input: Some(fidl_input_report::KeyboardInputDescriptor {
                                keys3: None,
                                ..fidl_input_report::KeyboardInputDescriptor::EMPTY
                            }),
                            output: None,
                            ..fidl_input_report::KeyboardDescriptor::EMPTY
                        }),
                        consumer_control: Some(fidl_input_report::ConsumerControlDescriptor {
                            input: Some(fidl_input_report::ConsumerControlInputDescriptor {
                                buttons: Some(vec![
                                    fidl_input_report::ConsumerControlButton::VolumeUp,
                                    fidl_input_report::ConsumerControlButton::VolumeDown,
                                ]),
                                ..fidl_input_report::ConsumerControlInputDescriptor::EMPTY
                            }),
                            ..fidl_input_report::ConsumerControlDescriptor::EMPTY
                        }),
                        ..fidl_input_report::DeviceDescriptor::EMPTY
                    });
                }
                _ => panic!("InputDevice handler received an unexpected request"),
            }
        })
        .unwrap();

        assert!(is_device_type(&input_device_proxy, InputDeviceType::MediaButtons).await);
        assert!(is_device_type(&input_device_proxy, InputDeviceType::Mouse).await);
        assert!(is_device_type(&input_device_proxy, InputDeviceType::Touch).await);
        assert!(is_device_type(&input_device_proxy, InputDeviceType::Keyboard).await);
    }
}
