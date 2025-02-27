// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::{
        client::{network_selection, sme_credential_from_policy, types},
        config_management::SavedNetworksManager,
        util::{
            listener::{
                ClientListenerMessageSender, ClientNetworkState, ClientStateUpdate,
                Message::NotifyListeners,
            },
            state_machine::{self, ExitReason, IntoStateExt},
        },
    },
    anyhow::format_err,
    fidl::endpoints::create_proxy,
    fidl_fuchsia_wlan_common as fidl_common, fidl_fuchsia_wlan_internal as fidl_internal,
    fidl_fuchsia_wlan_sme as fidl_sme,
    fuchsia_async::{self as fasync, DurationExt},
    fuchsia_cobalt::CobaltSender,
    fuchsia_zircon as zx,
    futures::{
        channel::{mpsc, oneshot},
        future::{self, FutureExt},
        select,
        stream::{self, FuturesUnordered, StreamExt, TryStreamExt},
    },
    log::{debug, error, info},
    std::sync::Arc,
    void::ResultVoidErrExt,
    wlan_common::RadioConfig,
    wlan_metrics_registry::{
        POLICY_CONNECTION_ATTEMPT_METRIC_ID as CONNECTION_ATTEMPT_METRIC_ID,
        POLICY_DISCONNECTION_METRIC_ID as DISCONNECTION_METRIC_ID,
    },
};

const SME_STATUS_INTERVAL_SEC: i64 = 1; // this poll is very cheap, so we can do it frequently
const MAX_CONNECTION_ATTEMPTS: u8 = 4; // arbitrarily chosen until we have some data

type State = state_machine::State<ExitReason>;
type ReqStream = stream::Fuse<mpsc::Receiver<ManualRequest>>;

pub trait ClientApi {
    fn connect(
        &mut self,
        request: types::ConnectRequest,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error>;
    fn disconnect(
        &mut self,
        reason: types::DisconnectReason,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error>;

    /// Queries the liveness of the channel used to control the client state machine.  If the
    /// channel is not alive, this indicates that the client state machine has exited.
    fn is_alive(&self) -> bool;
}

pub struct Client {
    req_sender: mpsc::Sender<ManualRequest>,
}

impl Client {
    pub fn new(req_sender: mpsc::Sender<ManualRequest>) -> Self {
        Self { req_sender }
    }
}

impl ClientApi for Client {
    fn connect(
        &mut self,
        request: types::ConnectRequest,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error> {
        self.req_sender
            .try_send(ManualRequest::Connect((request, responder)))
            .map_err(|e| format_err!("failed to send connect request: {:?}", e))
    }

    fn disconnect(
        &mut self,
        reason: types::DisconnectReason,
        responder: oneshot::Sender<()>,
    ) -> Result<(), anyhow::Error> {
        self.req_sender
            .try_send(ManualRequest::Disconnect((reason, responder)))
            .map_err(|e| format_err!("failed to send disconnect request: {:?}", e))
    }

    fn is_alive(&self) -> bool {
        !self.req_sender.is_closed()
    }
}

pub enum ManualRequest {
    Connect((types::ConnectRequest, oneshot::Sender<()>)),
    Disconnect((types::DisconnectReason, oneshot::Sender<()>)),
}

fn send_listener_state_update(
    sender: &ClientListenerMessageSender,
    network_update: ClientNetworkState,
) {
    let updates = ClientStateUpdate { state: None, networks: [network_update].to_vec() };
    match sender.clone().unbounded_send(NotifyListeners(updates)) {
        Ok(_) => (),
        Err(e) => error!("failed to send state update: {:?}", e),
    };
}

pub async fn serve(
    iface_id: u16,
    proxy: fidl_sme::ClientSmeProxy,
    sme_event_stream: fidl_sme::ClientSmeEventStream,
    req_stream: mpsc::Receiver<ManualRequest>,
    update_sender: ClientListenerMessageSender,
    saved_networks_manager: Arc<SavedNetworksManager>,
    connect_request: Option<(types::ConnectRequest, oneshot::Sender<()>)>,
    network_selector: Arc<network_selection::NetworkSelector>,
    cobalt_api: CobaltSender,
) {
    let next_network = match connect_request {
        Some((req, sender)) => Some(ConnectingOptions {
            connect_responder: Some(sender),
            connect_request: req,
            attempt_counter: 0,
        }),
        None => None,
    };
    let disconnect_options = DisconnectingOptions {
        disconnect_responder: None,
        previous_network: None,
        next_network,
        reason: types::DisconnectReason::Startup,
    };
    let common_options = CommonStateOptions {
        proxy: proxy,
        req_stream: req_stream.fuse(),
        update_sender: update_sender,
        saved_networks_manager: saved_networks_manager,
        network_selector,
        cobalt_api,
    };
    let state_machine =
        disconnecting_state(common_options, disconnect_options).into_state_machine();
    let removal_watcher = sme_event_stream.map_ok(|_| ()).try_collect::<()>();
    select! {
        state_machine = state_machine.fuse() => {
            match state_machine.void_unwrap_err() {
                ExitReason(Err(e)) => error!("Client state machine for iface #{} terminated with an error: {:?}",
                    iface_id, e),
                ExitReason(Ok(_)) => info!("Client state machine for iface #{} exited gracefully",
                    iface_id,),
            }
        }
        removal_watcher = removal_watcher.fuse() => if let Err(e) = removal_watcher {
            info!("Error reading from Client SME channel of iface #{}: {:?}",
                iface_id, e);
        },
    }
}

/// Common parameters passed to all states
struct CommonStateOptions {
    proxy: fidl_sme::ClientSmeProxy,
    req_stream: ReqStream,
    update_sender: ClientListenerMessageSender,
    saved_networks_manager: Arc<SavedNetworksManager>,
    network_selector: Arc<network_selection::NetworkSelector>,
    cobalt_api: CobaltSender,
}

fn handle_none_request() -> Result<State, ExitReason> {
    return Err(ExitReason(Err(format_err!("The stream of requests ended unexpectedly"))));
}

// These functions were introduced to resolve the following error:
// ```
// error[E0391]: cycle detected when evaluating trait selection obligation
// `impl core::future::future::Future: std::marker::Send`
// ```
// which occurs when two functions that return an `impl Trait` call each other
// in a cycle. (e.g. this case `connecting_state` calling `disconnecting_state`,
// which calls `connecting_state`)
fn to_disconnecting_state(
    common_options: CommonStateOptions,
    disconnecting_options: DisconnectingOptions,
) -> State {
    disconnecting_state(common_options, disconnecting_options).into_state()
}
fn to_connecting_state(
    common_options: CommonStateOptions,
    connecting_options: ConnectingOptions,
) -> State {
    connecting_state(common_options, connecting_options).into_state()
}

struct DisconnectingOptions {
    disconnect_responder: Option<oneshot::Sender<()>>,
    /// Information about the previously connected network, if there was one. Used to send out
    /// listener updates.
    previous_network: Option<(types::NetworkIdentifier, types::DisconnectStatus)>,
    /// Configuration for the next network to connect to, after the disconnect is complete. If not
    /// present, the state machine will proceed to IDLE.
    next_network: Option<ConnectingOptions>,
    reason: types::DisconnectReason,
}
/// The DISCONNECTING state requests an SME disconnect, then transitions to either:
/// - the CONNECTING state if options.next_network is present
/// - exit otherwise
async fn disconnecting_state(
    mut common_options: CommonStateOptions,
    options: DisconnectingOptions,
) -> Result<State, ExitReason> {
    // Log a message with the disconnect reason
    match options.reason {
        types::DisconnectReason::FailedToConnect
        | types::DisconnectReason::Startup
        | types::DisconnectReason::DisconnectDetectedFromSme => {
            // These are either just noise or have separate logging, so keep the level at debug.
            debug!("Disconnected due to {:?}", options.reason);
        }
        reason => {
            info!("Disconnected due to {:?}", reason);
        }
    }

    // TODO(fxbug.dev/53505): either make this fire-and-forget in the SME, or spawn a thread for this,
    // so we don't block on it
    common_options
        .proxy
        .disconnect(types::convert_to_sme_disconnect_reason(options.reason))
        .await
        .map_err(|e| {
            ExitReason(Err(format_err!("Failed to send command to wlanstack: {:?}", e)))
        })?;

    // Notify the caller that disconnect was sent to the SME
    match options.disconnect_responder {
        Some(responder) => responder.send(()).unwrap_or_else(|_| ()),
        None => (),
    }

    // Notify listeners of disconnection
    match options.previous_network {
        Some((network_identifier, status)) => send_listener_state_update(
            &common_options.update_sender,
            ClientNetworkState {
                id: network_identifier,
                state: types::ConnectionState::Disconnected,
                status: Some(status),
            },
        ),
        None => (),
    }

    // Log a disconnect in Cobalt
    common_options.cobalt_api.log_event(DISCONNECTION_METRIC_ID, options.reason);

    // Transition to next state
    match options.next_network {
        Some(next_network) => Ok(to_connecting_state(common_options, next_network)),
        None => Err(ExitReason(Ok(()))),
    }
}

async fn wait_until_connected(
    txn: fidl_sme::ConnectTransactionProxy,
) -> Result<fidl_sme::ConnectResultCode, anyhow::Error> {
    let mut stream = txn.take_event_stream();
    while let Some(event) = stream.try_next().await? {
        match event {
            fidl_sme::ConnectTransactionEvent::OnFinished { code } => return Ok(code),
        }
    }
    Err(format_err!("Server closed the ConnectTransaction channel before sending a response"))
}

struct ConnectingOptions {
    connect_responder: Option<oneshot::Sender<()>>,
    connect_request: types::ConnectRequest,
    /// Count of previous consecutive failed connection attempts to this same network.
    attempt_counter: u8,
}

type MultipleBssCandidates = bool;
enum SmeOperation {
    ConnectResult(Result<(fidl_sme::ConnectResultCode, Box<types::Bssid>), anyhow::Error>),
    ScanResult(Option<(Box<fidl_internal::BssDescription>, MultipleBssCandidates)>),
}

async fn handle_connecting_error_and_retry(
    common_options: CommonStateOptions,
    options: ConnectingOptions,
) -> Result<State, ExitReason> {
    // Check if the limit for connection attempts to this network has been
    // exceeded.
    let new_attempt_count = options.attempt_counter + 1;
    if new_attempt_count >= MAX_CONNECTION_ATTEMPTS {
        info!("Exceeded maximum connection attempts, will not retry");
        send_listener_state_update(
            &common_options.update_sender,
            ClientNetworkState {
                id: options.connect_request.target.network,
                state: types::ConnectionState::Failed,
                status: Some(types::DisconnectStatus::ConnectionFailed),
            },
        );
        return Err(ExitReason(Err(format_err!("exceeded connection attempt limit"))));
    } else {
        // Limit not exceeded, retry after backing off.
        let backoff_time = 400_i64 * i64::from(new_attempt_count);
        info!("Will attempt to reconnect after {}ms backoff", backoff_time);
        fasync::Timer::new(zx::Duration::from_millis(backoff_time).after_now()).await;

        let next_connecting_options = ConnectingOptions {
            connect_responder: None,
            connect_request: types::ConnectRequest {
                reason: types::ConnectReason::RetryAfterFailedConnectAttempt,
                ..options.connect_request
            },
            attempt_counter: new_attempt_count,
        };
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: None,
            previous_network: None,
            next_network: Some(next_connecting_options),
            reason: types::DisconnectReason::FailedToConnect,
        };
        return Ok(to_disconnecting_state(common_options, disconnecting_options));
    }
}

/// The CONNECTING state checks for the required bss information in the connection request. If not
/// present, the state first requests an SME scan. For a failed scan, retry connection by passing a
/// next_network to the DISCONNECTING state, as long as there haven't been too many attempts.
/// Next, it requests an SME connect. It handles the SME connect response:
/// - for a successful connection, transition to CONNECTED state
/// - for a failed connection, retry connection by passing a next_network to the
///       DISCONNECTING state, as long as there haven't been too many connection attempts
/// During this time, incoming ManualRequests are also monitored for:
/// - duplicate connect requests are deduped
/// - different connect requests are serviced by passing a next_network to the DISCONNECTING state
/// - disconnect requests cause a transition to DISCONNECTING state
async fn connecting_state<'a>(
    mut common_options: CommonStateOptions,
    mut options: ConnectingOptions,
) -> Result<State, ExitReason> {
    debug!("Entering connecting state");

    if options.attempt_counter > 0 {
        info!(
            "Retrying connection, {} attempts remaining",
            MAX_CONNECTION_ATTEMPTS - options.attempt_counter
        );
    }

    // Send a "Connecting" update to listeners, unless this is a retry
    if options.attempt_counter == 0 {
        send_listener_state_update(
            &common_options.update_sender,
            ClientNetworkState {
                id: options.connect_request.target.network.clone(),
                state: types::ConnectionState::Connecting,
                status: None,
            },
        );
    };

    // Log a connect attempt in Cobalt
    common_options
        .cobalt_api
        .log_event(CONNECTION_ATTEMPT_METRIC_ID, options.connect_request.reason);

    // Let the responder know we've successfully started this connection attempt
    match options.connect_responder.take() {
        Some(responder) => responder.send(()).unwrap_or_else(|_| ()),
        None => {}
    }

    // If detailed bss information was not provided, perform a scan to discover it
    let network_selector = common_options.network_selector.clone();
    let scan_future = match options.connect_request.target.bss {
        Some(ref bss_desc) => {
            let multiple_bss_candidates =
                options.connect_request.target.multiple_bss_candidates.unwrap_or_else(|| {
                    // Where target.bss.is_some(), multiple_bss_candidates will always be Some as well.
                    error!("multiple_bss_candidates is expected to always be set");
                    true
                });
            future::ready(SmeOperation::ScanResult(Some((
                bss_desc.clone(),
                multiple_bss_candidates,
            ))))
            .boxed()
        }
        None => {
            info!("Connection requested, scanning to find a BSS for the network");
            network_selector
                .find_connection_candidate_for_network(
                    common_options.proxy.clone(),
                    options.connect_request.target.network.clone(),
                )
                .map(|find_result| {
                    SmeOperation::ScanResult(
                        find_result
                            .map(
                                |types::ConnectionCandidate {
                                     bss,
                                     multiple_bss_candidates,
                                     ..
                                 }| {
                                    bss.map(|bss| (bss, multiple_bss_candidates.unwrap_or(true)))
                                },
                            )
                            .unwrap_or(None),
                    )
                })
                .boxed()
        }
    };
    let mut internal_futures = FuturesUnordered::new();
    internal_futures.push(scan_future);

    loop {
        select! {
            // Monitor the SME operations
            completed_future = internal_futures.select_next_some() => match completed_future {
                SmeOperation::ScanResult(bss_info) => {
                    let bss_info = match bss_info {
                        Some(bss_info) => bss_info,
                        None => {
                            info!("Failed to find a BSS to connect to.");
                            return handle_connecting_error_and_retry(common_options, options).await;
                        }
                    };
                    let bssid = Box::new(bss_info.0.bssid.clone());
                    // Send a connect request to the SME
                    let (connect_txn, remote) = create_proxy()
                        .map_err(|e| ExitReason(Err(format_err!("Failed to create proxy: {:?}", e))))?;
                    let mut sme_connect_request = fidl_sme::ConnectRequest {
                        ssid: options.connect_request.target.network.ssid.clone(),
                        bss_desc: Some(bss_info.0),
                        multiple_bss_candidates: bss_info.1,
                        credential: sme_credential_from_policy(&options.connect_request.target.credential),
                        radio_cfg: RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl(),
                        deprecated_scan_type: fidl_fuchsia_wlan_common::ScanType::Active,
                    };
                    common_options.proxy.connect(&mut sme_connect_request, Some(remote)).map_err(|e| {
                        ExitReason(Err(format_err!("Failed to send command to wlanstack: {:?}", e)))
                    })?;
                    let pending_connect_request = wait_until_connected(connect_txn)
                        .map(|res| {
                            let result = res.map(|res| (res, bssid));
                            SmeOperation::ConnectResult(result)
                        })
                        .boxed();
                    internal_futures.push(pending_connect_request);
                },
                SmeOperation::ConnectResult(connect_result) => {
                    let (code, bssid) = connect_result.map_err({
                        |e| ExitReason(Err(format_err!("failed to send connect to sme: {:?}", e)))
                    })?;
                    // Notify the saved networks manager. observed_in_passive_scan will be false if
                    // network was seen in active scan, or None if no scan was performed.
                    let scan_type =
                        options.connect_request.target.observed_in_passive_scan.map(|observed_in_passive_scan| {
                            if observed_in_passive_scan {fidl_common::ScanType::Passive}
                            else {fidl_common::ScanType::Active}
                        });
                    common_options.saved_networks_manager.record_connect_result(
                        options.connect_request.target.network.clone().into(),
                        &options.connect_request.target.credential,
                        *bssid,
                        code,
                        scan_type
                    ).await;

                    match code {
                        fidl_sme::ConnectResultCode::Success => {
                            info!("Successfully connected to network");
                            send_listener_state_update(
                                &common_options.update_sender,
                                ClientNetworkState {
                                    id: options.connect_request.target.network.clone(),
                                    state: types::ConnectionState::Connected,
                                    status: None
                                },
                            );
                            return Ok(
                                connected_state(common_options, options.connect_request).into_state()
                            );
                        },
                        fidl_sme::ConnectResultCode::CredentialRejected => {
                            info!("Failed to connect. Will not retry because of credential error: {:?}", code);
                            send_listener_state_update(
                                &common_options.update_sender,
                                ClientNetworkState {
                                    id: options.connect_request.target.network,
                                    state: types::ConnectionState::Failed,
                                    status: Some(types::DisconnectStatus::CredentialsFailed),
                                },
                            );
                            return Err(ExitReason(Err(format_err!("bad credentials"))));
                        },
                        other => {
                            info!("Failed to connect: {:?}", other);
                            return handle_connecting_error_and_retry(common_options, options).await;
                        }
                    };
                },
            },
            // Monitor incoming ManualRequests
            new_req = common_options.req_stream.next() => match new_req {
                Some(ManualRequest::Disconnect((reason, responder))) => {
                    info!("Cancelling pending connect due to disconnect request");
                    send_listener_state_update(
                        &common_options.update_sender,
                        ClientNetworkState {
                            id: options.connect_request.target.network,
                            state: types::ConnectionState::Disconnected,
                            status: Some(types::DisconnectStatus::ConnectionStopped)
                        },
                    );
                    let options = DisconnectingOptions {
                        disconnect_responder: Some(responder),
                        previous_network: None,
                        next_network: None,
                        reason
                    };
                    return Ok(to_disconnecting_state(common_options, options));
                }
                Some(ManualRequest::Connect((new_connect_request, new_responder))) => {
                    // Check if it's the same network as we're currently connected to.
                    // If yes, dedupe the request.
                    if new_connect_request.target.network == options.connect_request.target.network {
                        info!("Received duplicate connection request, deduping");
                        new_responder.send(()).unwrap_or_else(|_| ());
                    } else {
                        info!("Cancelling pending connect due to new connection request");
                        send_listener_state_update(
                            &common_options.update_sender,
                            ClientNetworkState {
                                id: options.connect_request.target.network,
                                state: types::ConnectionState::Disconnected,
                                status: Some(types::DisconnectStatus::ConnectionStopped)
                            },
                        );
                        let next_connecting_options = ConnectingOptions {
                            connect_responder: Some(new_responder),
                            connect_request: new_connect_request.clone(),
                            attempt_counter: 0,
                        };
                        let disconnecting_options = DisconnectingOptions {
                            disconnect_responder: None,
                            previous_network: None,
                            next_network: Some(next_connecting_options),
                            reason: match new_connect_request.reason {
                                types::ConnectReason::ProactiveNetworkSwitch => types::DisconnectReason::ProactiveNetworkSwitch,
                                types::ConnectReason::FidlConnectRequest => types::DisconnectReason::FidlConnectRequest,
                                _ => {
                                    error!("Unexpected connection reason: {:?}", new_connect_request.reason);
                                    types::DisconnectReason::Unknown
                                }
                            }
                        };
                        return Ok(to_disconnecting_state(common_options, disconnecting_options));
                    }
                }
                None => return handle_none_request(),
            },
        }
    }
}

/// The CONNECTED state monitors the SME status. It handles the SME status response:
/// - if still connected to the correct network, no action
/// - if disconnected, retry connection by passing a next_network to the
///       DISCONNECTING state
/// During this time, incoming ManualRequests are also monitored for:
/// - duplicate connect requests are deduped
/// - different connect requests are serviced by passing a next_network to the DISCONNECTING state
/// - disconnect requests cause a transition to DISCONNECTING state
async fn connected_state(
    mut common_options: CommonStateOptions,
    currently_fulfilled_request: types::ConnectRequest,
) -> Result<State, ExitReason> {
    debug!("Entering connected state");

    // TODO(fxbug.dev/57237): replace this poll with a notification from wlanstack in the ConnectTxn
    // Holds a pending SME status request.  Request status immediately upon entering the started state.
    let mut pending_status_req = FuturesUnordered::new();
    pending_status_req.push(common_options.proxy.status());

    let mut status_timer =
        fasync::Interval::new(zx::Duration::from_seconds(SME_STATUS_INTERVAL_SEC));

    loop {
        select! {
            status_response = pending_status_req.select_next_some() => {
                let status_response = status_response.map_err(|e| {
                    ExitReason(Err(format_err!("failed to get sme status: {:?}", e)))
                })?;
                // Check if we're still connected to a network.
                match status_response.connected_to {
                    Some(bss_info) => {
                        // TODO(fxbug.dev/53545): send some stats to the saved network manager
                        if bss_info.ssid != currently_fulfilled_request.target.network.ssid {
                            error!("Currently connected SSID changed unexpectedly");
                            return Err(ExitReason(Err(format_err!("Currently connected SSID changed unexpectedly"))));
                        }
                    }
                    None => {
                        // We're no longer connected to a network. When the SME layer detects a
                        // Disassociation, it will automatically attempt a reconnection. We can
                        // check for that scenario via the `connecting_to_ssid` field.
                        if status_response.connecting_to_ssid == currently_fulfilled_request.target.network.ssid {
                            info!("Detected disconnection, SME layer is reconnecting to network");
                            // Log a disconnect in Cobalt
                            common_options.cobalt_api.log_event(
                                DISCONNECTION_METRIC_ID,
                                types::DisconnectReason::DisconnectDetectedFromSme
                            );
                            // TODO(fxbug.dev/53545): record this blip in connectivity
                            // TODO(fxbug.dev/67605): record a re-connection metric, if successful
                        } else {
                            let next_connecting_options = ConnectingOptions {
                                connect_responder: None,
                                connect_request: types::ConnectRequest {
                                    reason: types::ConnectReason::RetryAfterDisconnectDetected,
                                    target: types::ConnectionCandidate {
                                        // strip out the bss info to force a new scan
                                        bss: None,
                                        observed_in_passive_scan: None,
                                        ..currently_fulfilled_request.target.clone()
                                    }
                                },
                                attempt_counter: 0,
                            };
                            let options = DisconnectingOptions {
                                disconnect_responder: None,
                                previous_network: Some((currently_fulfilled_request.target.network.clone(), types::DisconnectStatus::ConnectionFailed)),
                                next_network: Some(next_connecting_options),
                                reason: types::DisconnectReason::DisconnectDetectedFromSme
                            };
                            info!("Detected disconnection from network, will attempt reconnection");
                            return Ok(disconnecting_state(common_options, options).into_state());
                        }
                    }
                }
            },
            _ = status_timer.select_next_some() => {
                if pending_status_req.is_empty() {
                    pending_status_req.push(common_options.proxy.clone().status());
                }
            },
            req = common_options.req_stream.next() => {
                match req {
                    Some(ManualRequest::Disconnect((reason, responder))) => {
                        debug!("Disconnect requested");
                        let options = DisconnectingOptions {
                            disconnect_responder: Some(responder),
                            previous_network: Some((currently_fulfilled_request.target.network, types::DisconnectStatus::ConnectionStopped)),
                            next_network: None,
                            reason
                        };
                        return Ok(disconnecting_state(common_options, options).into_state());
                    }
                    Some(ManualRequest::Connect((new_connect_request, new_responder))) => {
                        // Check if it's the same network as we're currently connected to. If yes, reply immediately
                        if new_connect_request.target.network == currently_fulfilled_request.target.network {
                            info!("Received connection request for current network, deduping");
                            new_responder.send(()).unwrap_or_else(|_| ());
                        } else {
                            let next_connecting_options = ConnectingOptions {
                                connect_responder: Some(new_responder),
                                connect_request: new_connect_request.clone(),
                                attempt_counter: 0,
                            };
                            let options = DisconnectingOptions {
                                disconnect_responder: None,
                                previous_network: Some((currently_fulfilled_request.target.network, types::DisconnectStatus::ConnectionStopped)),
                                next_network: Some(next_connecting_options),
                                reason: match new_connect_request.reason {
                                    types::ConnectReason::ProactiveNetworkSwitch => types::DisconnectReason::ProactiveNetworkSwitch,
                                    types::ConnectReason::FidlConnectRequest => types::DisconnectReason::FidlConnectRequest,
                                    _ => {
                                        error!("Unexpected connection reason: {:?}", new_connect_request.reason);
                                        types::DisconnectReason::Unknown
                                    }
                                }
                            };
                            info!("Connection to new network requested, disconnecting from current network");
                            return Ok(disconnecting_state(common_options, options).into_state())
                        }
                    }
                    None => return handle_none_request(),
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            config_management::network_config::{self, Credential, FailureReason},
            util::{
                listener,
                logger::set_logger_for_test,
                testing::{
                    create_mock_cobalt_sender, create_mock_cobalt_sender_and_receiver,
                    generate_random_bss_desc, generate_random_bss_info, poll_sme_req,
                    validate_sme_scan_request_and_send_results,
                },
            },
            validate_cobalt_events, validate_no_cobalt_events,
        },
        cobalt_client::traits::AsEventCode,
        fidl_fuchsia_cobalt::CobaltEvent,
        fidl_fuchsia_stash as fidl_stash, fidl_fuchsia_wlan_common as fidl_common,
        fidl_fuchsia_wlan_policy as fidl_policy,
        fuchsia_cobalt::CobaltEventExt,
        fuchsia_inspect::{self as inspect},
        fuchsia_zircon,
        futures::{task::Poll, Future},
        pin_utils::pin_mut,
        rand::{distributions::Alphanumeric, thread_rng, Rng},
        test_case::test_case,
        wlan_common::assert_variant,
        wlan_metrics_registry::PolicyDisconnectionMetricDimensionReason,
    };

    struct TestValues {
        common_options: CommonStateOptions,
        sme_req_stream: fidl_sme::ClientSmeRequestStream,
        client_req_sender: mpsc::Sender<ManualRequest>,
        update_receiver: mpsc::UnboundedReceiver<listener::ClientListenerMessage>,
        cobalt_events: mpsc::Receiver<CobaltEvent>,
    }

    async fn test_setup() -> TestValues {
        set_logger_for_test();
        let (client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let saved_networks_manager = Arc::new(
            SavedNetworksManager::new_for_test()
                .await
                .expect("Failed to create saved networks manager"),
        );
        let (cobalt_api, cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));

        TestValues {
            common_options: CommonStateOptions {
                proxy: sme_proxy,
                req_stream: client_req_stream.fuse(),
                update_sender: update_sender,
                saved_networks_manager: saved_networks_manager,
                network_selector,
                cobalt_api,
            },
            sme_req_stream,
            client_req_sender,
            update_receiver,
            cobalt_events,
        }
    }

    async fn run_state_machine(
        fut: impl Future<Output = Result<State, ExitReason>> + Send + 'static,
    ) {
        let state_machine = fut.into_state_machine();
        select! {
            _state_machine = state_machine.fuse() => return,
        }
    }

    /// Move stash requests forward so that a save request can progress.
    fn process_stash_write(
        exec: &mut fasync::TestExecutor,
        stash_server: &mut fidl_stash::StoreAccessorRequestStream,
    ) {
        assert_variant!(
            exec.run_until_stalled(&mut stash_server.try_next()),
            Poll::Ready(Ok(Some(fidl_stash::StoreAccessorRequest::SetValue { .. })))
        );
        assert_variant!(
            exec.run_until_stalled(&mut stash_server.try_next()),
            Poll::Ready(Ok(Some(fidl_stash::StoreAccessorRequest::Flush{responder}))) => {
                responder.send(&mut Ok(())).expect("failed to send stash response");
            }
        );
    }

    fn rand_string() -> String {
        thread_rng().sample_iter(&Alphanumeric).take(20).collect()
    }

    #[test]
    fn connecting_state_successfully_connects() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        set_logger_for_test();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let temp_dir = tempfile::TempDir::new().expect("failed to create temporary directory");
        let path = temp_dir.path().join(rand_string());
        let tmp_path = temp_dir.path().join(rand_string());
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server(path, tmp_path));
        let saved_networks_manager = Arc::new(saved_networks);
        let (cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));
        let next_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wep,
                },
                credential: Credential::Password("five0".as_bytes().to_vec()),
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        // Check that the saved networks manager has the expected initial data
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(connect_request.target.network.clone().into()),
        );
        assert_eq!(false, saved_networks[0].has_ever_connected);
        assert!(saved_networks[0].hidden_probability > 0.0);

        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            cobalt_api: cobalt_api,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential));
                assert_eq!(req.bss_desc, bss_desc);
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, true);
                // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that the saved networks manager has the connection recorded
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(connect_request.target.network.clone().into()),
        );
        assert_eq!(true, saved_networks[0].has_ever_connected);
        assert_eq!(
            network_config::PROB_HIDDEN_IF_CONNECT_PASSIVE,
            saved_networks[0].hidden_probability
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(exec.run_until_stalled(&mut update_receiver.into_future()), Poll::Pending);

        // Cobalt metrics logged
        validate_cobalt_events!(
            cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::FidlConnectRequest
        );
    }

    #[test]
    fn connecting_state_successfully_scans_and_connects() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        set_logger_for_test();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let mut sme_req_stream =
            sme_server.into_stream().expect("could not create SME request stream");
        let temp_dir = tempfile::TempDir::new().expect("failed to create temporary directory");
        let path = temp_dir.path().join(rand_string());
        let tmp_path = temp_dir.path().join(rand_string());
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server(path, tmp_path));
        let saved_networks_manager = Arc::new(saved_networks);
        let (cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));
        let next_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wep,
                },
                credential: Credential::Password("12345".as_bytes().to_vec()),
                observed_in_passive_scan: None,
                bss: None,
                multiple_bss_candidates: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        // Check that the saved networks manager has the expected initial data
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(connect_request.target.network.clone().into()),
        );
        assert_eq!(false, saved_networks[0].has_ever_connected);
        assert!(saved_networks[0].hidden_probability > 0.0);

        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            cobalt_api: cobalt_api,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a scan request is sent to the SME and send back a result
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![next_network_ssid.as_bytes().to_vec()],
            channels: vec![],
        });
        let scan_results = vec![fidl_sme::BssInfo {
            ssid: next_network_ssid.as_bytes().to_vec(),
            bss_desc: bss_desc.clone(),
            compatible: true,
            protection: fidl_sme::Protection::Wep,
            ..generate_random_bss_info()
        }];
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_req_stream,
            &expected_scan_request,
            scan_results,
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential));
                assert_eq!(req.bss_desc, bss_desc);
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, false);
                // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wep,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that the saved networks manager has the connection recorded
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(connect_request.target.network.clone().into()),
        );
        assert_eq!(true, saved_networks[0].has_ever_connected);
        assert_eq!(network_config::PROB_HIDDEN_DEFAULT, saved_networks[0].hidden_probability);

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(exec.run_until_stalled(&mut update_receiver.into_future()), Poll::Pending);

        // Cobalt metrics logged
        validate_cobalt_events!(
            cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::FidlConnectRequest
        );
    }

    #[test_case(types::SecurityType::Wpa3)]
    #[test_case(types::SecurityType::Wpa2)]
    fn connecting_state_successfully_connects_wpa2wpa3(type_: types::SecurityType) {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        set_logger_for_test();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, _update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let mut sme_req_stream =
            sme_server.into_stream().expect("could not create SME request stream");
        let temp_dir = tempfile::TempDir::new().expect("failed to create temporary directory");
        let path = temp_dir.path().join(rand_string());
        let tmp_path = temp_dir.path().join(rand_string());
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server(path, tmp_path));
        let saved_networks_manager = Arc::new(saved_networks);
        let (cobalt_api, _cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));
        let next_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.as_bytes().to_vec(),
                    type_,
                },
                credential: Credential::Password("Anything".as_bytes().to_vec()),
                observed_in_passive_scan: None,
                bss: None,
                multiple_bss_candidates: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        // Check that the saved networks manager has the expected initial data
        let saved_networks = exec.run_singlethreaded(
            saved_networks_manager.lookup(connect_request.target.network.clone().into()),
        );
        assert_eq!(false, saved_networks[0].has_ever_connected);
        assert!(saved_networks[0].hidden_probability > 0.0);

        let (connect_sender, _connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            cobalt_api: cobalt_api,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a scan request is sent to the SME and send back a result
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![next_network_ssid.as_bytes().to_vec()],
            channels: vec![],
        });
        let scan_results = vec![fidl_sme::BssInfo {
            ssid: next_network_ssid.as_bytes().to_vec(),
            bss_desc: bss_desc.clone(),
            compatible: true,
            protection: fidl_sme::Protection::Wpa2Wpa3Personal,
            ..generate_random_bss_info()
        }];
        validate_sme_scan_request_and_send_results(
            &mut exec,
            &mut sme_req_stream,
            &expected_scan_request,
            scan_results,
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure the mixed network was selected for connection and a connect request is sent to
        // the SME.
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, .. }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential));
                assert_eq!(req.bss_desc, bss_desc);
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
            }
        );
    }

    #[test]
    fn connecting_state_fails_to_connect_and_retries() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let next_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(false),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Failed)
                    .expect("failed to send connection completion");
            }
        );

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert!(exec.wake_next_timer().is_some());
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FailedToConnect }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.bss_desc, bss_desc);
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a connected update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );

        // Three cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::FidlConnectRequest
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::FailedToConnect
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::RetryAfterFailedConnectAttempt
        );
    }

    #[test]
    fn connecting_state_fails_to_scan_and_retries() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        set_logger_for_test();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let temp_dir = tempfile::TempDir::new().expect("failed to create temporary directory");
        let path = temp_dir.path().join(rand_string());
        let tmp_path = temp_dir.path().join(rand_string());
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server(path, tmp_path));
        let saved_networks_manager = Arc::new(saved_networks);
        let (cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));
        let next_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::Password("Anything".as_bytes().to_vec()),
                observed_in_passive_scan: None,
                bss: None,
                multiple_bss_candidates: None,
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            cobalt_api: cobalt_api,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a scan request is sent to the SME
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan{ txn, .. }) => {
                // Send failed scan response.
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_error(&mut fidl_sme::ScanError {
                    code: fidl_sme::ScanErrorCode::InternalError,
                    message: "Failed to scan".to_string()
                })
                    .expect("failed to send scan error");
            }
        );

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert!(exec.wake_next_timer().is_some());
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FailedToConnect }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a new scan request is sent to the SME
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![next_network_ssid.as_bytes().to_vec()],
            channels: vec![],
        });
        let mut scan_results = vec![fidl_sme::BssInfo {
            ssid: next_network_ssid.as_bytes().to_vec(),
            bss_desc: bss_desc.clone(),
            compatible: true,
            protection: fidl_sme::Protection::Wpa2Personal,
            ..generate_random_bss_info()
        }];
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan {
                txn, req, control_handle: _
            }) => {
                // Validate the request
                assert_eq!(req, expected_scan_request);
                // Send all the APs
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_result(&mut scan_results.iter_mut())
                    .expect("failed to send scan data");

                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential));
                assert_eq!(req.bss_desc, bss_desc);
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Cobalt metrics logged
        validate_cobalt_events!(
            cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::FidlConnectRequest
        );
        validate_cobalt_events!(
            cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::FailedToConnect
        );
        validate_cobalt_events!(
            cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::RetryAfterFailedConnectAttempt
        );
    }

    #[test]
    fn connecting_state_fails_to_connect_at_max_retries() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Don't use test_values() because of issue with KnownEssStore
        set_logger_for_test();
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let stash_id = "connecting_state_fails_to_connect_at_max_retries";
        let temp_dir = tempfile::TempDir::new().expect("failed to create temporary directory");
        let path = temp_dir.path().join("networks.json");
        let tmp_path = temp_dir.path().join("tmp.json");
        let saved_networks_manager = Arc::new(
            exec.run_singlethreaded(SavedNetworksManager::new_with_stash_or_paths(
                stash_id,
                path,
                tmp_path,
                create_mock_cobalt_sender(),
            ))
            .expect("Failed to create saved networks manager"),
        );
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));
        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            cobalt_api: cobalt_api,
        };

        let next_network_ssid = "bar";
        let next_security_type = types::SecurityType::None;
        let next_credential = Credential::None;
        let next_network_identifier = types::NetworkIdentifier {
            ssid: next_network_ssid.as_bytes().to_vec(),
            type_: next_security_type,
        };
        let config_net_id =
            network_config::NetworkIdentifier::from(next_network_identifier.clone());
        let bss_desc = generate_random_bss_desc();
        // save network to check that failed connect is recorded
        assert!(exec
            .run_singlethreaded(
                saved_networks_manager.store(config_net_id.clone(), next_credential.clone()),
            )
            .expect("Failed to save network")
            .is_none());
        let before_recording = zx::Time::get_monotonic();

        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: next_network_identifier,
                credential: next_credential,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: MAX_CONNECTION_ATTEMPTS - 1,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential));
                assert_eq!(req.bss_desc, bss_desc.clone());
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Failed)
                    .expect("failed to send connection completion");
            }
        );

        // After failing to reconnect, the state machine should exit so that the state machine
        // monitor can attempt to reconnect the interface.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: next_security_type,
                },
                state: fidl_policy::ConnectionState::Failed,
                status: Some(fidl_policy::DisconnectStatus::ConnectionFailed),
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that failure was recorded in SavedNetworksManager
        let mut configs = exec.run_singlethreaded(saved_networks_manager.lookup(config_net_id));
        let network_config = configs.pop().expect("Failed to get saved network");
        let mut failures = network_config.perf_stats.failure_list.get_recent(before_recording);
        let connect_failure = failures.pop().expect("Saved network is missing failure reason");
        assert_eq!(connect_failure.reason, FailureReason::GeneralFailure);

        // Cobalt metrics logged
        validate_cobalt_events!(
            cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::FidlConnectRequest
        );
    }

    #[test]
    fn connecting_state_fails_to_connect_with_bad_credentials() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Don't use test_values() because of issue with KnownEssStore
        set_logger_for_test();
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let stash_id = "connecting_state_fails_to_connect_with_bad_credentials";
        let temp_dir = tempfile::TempDir::new().expect("failed to create temporary directory");
        let path = temp_dir.path().join("networks.json");
        let tmp_path = temp_dir.path().join("tmp.json");
        let saved_networks_manager = Arc::new(
            exec.run_singlethreaded(SavedNetworksManager::new_with_stash_or_paths(
                stash_id,
                path,
                tmp_path,
                create_mock_cobalt_sender(),
            ))
            .expect("Failed to create saved networks manager"),
        );
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));

        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            cobalt_api: cobalt_api,
        };

        let next_network_ssid = "bar";
        let next_network_identifier = types::NetworkIdentifier {
            ssid: next_network_ssid.as_bytes().to_vec(),
            type_: types::SecurityType::Wpa2,
        };
        let config_net_id =
            network_config::NetworkIdentifier::from(next_network_identifier.clone());
        let next_credential = Credential::Password("password".as_bytes().to_vec());
        // save network to check that failed connect is recorded
        let saved_networks_manager = common_options.saved_networks_manager.clone();
        assert!(exec
            .run_singlethreaded(
                saved_networks_manager.store(config_net_id.clone(), next_credential.clone()),
            )
            .expect("Failed to save network")
            .is_none());
        let before_recording = zx::Time::get_monotonic();

        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: next_network_identifier,
                credential: next_credential,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: MAX_CONNECTION_ATTEMPTS - 1,
        };
        let initial_state = connecting_state(common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential));
                assert_eq!(req.bss_desc, bss_desc.clone());
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::CredentialRejected)
                    .expect("failed to send connection completion");
            }
        );

        // The state machine should exit when bad credentials are detected so that the state
        // machine monitor can try to connect to another network.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Failed,
                status: Some(fidl_policy::DisconnectStatus::CredentialsFailed),
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check that failure was recorded in SavedNetworksManager
        let mut configs = exec.run_singlethreaded(saved_networks_manager.lookup(config_net_id));
        let network_config = configs.pop().expect("Failed to get saved network");
        let mut failures = network_config.perf_stats.failure_list.get_recent(before_recording);
        let connect_failure = failures.pop().expect("Saved network is missing failure reason");
        assert_eq!(connect_failure.reason, FailureReason::CredentialRejected);

        // Cobalt metrics logged
        validate_cobalt_events!(
            cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::ProactiveNetworkSwitch
        );
    }

    #[test]
    fn connecting_state_gets_duplicate_connect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let next_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Send a duplicate connect request
        let mut client = Client::new(test_values.client_req_sender);
        let (connect_sender2, mut connect_receiver2) = oneshot::channel();
        let duplicate_request = types::ConnectRequest {
            target: {
                types::ConnectionCandidate {
                    bss: None, // this incoming request should be deduped regardless of the bss info
                    ..connect_request.clone().target
                }
            },
            // this incoming request should be deduped regardless of the reason
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        client.connect(duplicate_request, connect_sender2).expect("failed to make request");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver2), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential.clone()));
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.bss_desc, bss_desc);
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a connect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(next_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::RegulatoryChangeReconnect
        );
        validate_no_cobalt_events!(test_values.cobalt_events);
    }

    #[test]
    fn connecting_state_gets_different_connect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let first_network_ssid = "foo";
        let second_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: first_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(first_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Send a different connect request
        let mut client = Client::new(test_values.client_req_sender);
        let (connect_sender2, mut connect_receiver2) = oneshot::channel();
        let bss_desc2 = generate_random_bss_desc();
        let connect_request2 = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: second_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc2.clone(),
                multiple_bss_candidates: Some(false),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        client.connect(connect_request2.clone(), connect_sender2).expect("failed to make request");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(first_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // There should be 3 requests to the SME stacked up
        // First SME request: connect to the first network
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn: _, control_handle: _ }) => {
                assert_eq!(req.ssid, first_network_ssid.as_bytes().to_vec());
                assert_eq!(req.bss_desc, bss_desc.clone());
                // Don't bother sending response, listener is gone
            }
        );
        // Second SME request: disconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FidlConnectRequest }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // Progress the state machine
        // TODO(fxbug.dev/53505): remove this once the disconnect request is fire-and-forget
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        // Third SME request: connect to the second network
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, second_network_ssid.as_bytes().to_vec());
                assert_eq!(req.bss_desc, bss_desc2.clone());
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver2), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(second_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        // Check for a connected update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(second_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::RegulatoryChangeReconnect
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::FidlConnectRequest
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::FidlConnectRequest
        );
    }

    #[test]
    fn connecting_state_gets_disconnect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let first_network_ssid = "foo";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: first_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(first_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Send a disconnect request
        let mut client = Client::new(test_values.client_req_sender);
        let (disconnect_sender, mut disconnect_receiver) = oneshot::channel();
        client
            .disconnect(types::DisconnectReason::NetworkUnsaved, disconnect_sender)
            .expect("failed to make request");

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(first_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // There should be 2 requests to the SME stacked up
        // First SME request: connect to the first network
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn: _, control_handle: _ }) => {
                assert_eq!(req.ssid, first_network_ssid.as_bytes().to_vec());
                assert_eq!(req.bss_desc, bss_desc.clone());
                // Don't bother sending response, listener is gone
            }
        );
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::NetworkUnsaved }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // The state machine should exit after processing the disconnect.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check the disconnect responder
        assert_variant!(exec.run_until_stalled(&mut disconnect_receiver), Poll::Ready(Ok(())));

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::RegulatoryChangeReconnect
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::NetworkUnsaved
        );
    }

    #[test]
    fn connecting_state_has_broken_sme() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = exec.run_singlethreaded(test_setup());

        let first_network_ssid = "foo";
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: first_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: None,
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (connect_sender, mut connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let initial_state = connecting_state(test_values.common_options, connecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Break the SME by dropping the server end of the SME stream, so it causes an error
        drop(test_values.sme_req_stream);

        // Ensure the state machine exits
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert!(exec.wake_next_timer().is_some());
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Expect the responder to have a success, since the connection was attempted
        assert_variant!(exec.run_until_stalled(&mut connect_receiver), Poll::Ready(Ok(())));
    }

    #[test]
    fn connected_state_gets_disconnect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let network_ssid = "test";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let initial_state = connected_state(test_values.common_options, connect_request);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Clear the SME status request
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ responder }) => {
                responder.send(&mut fidl_sme::ClientStatusResponse{
                    connecting_to_ssid: vec![],
                    connected_to: Some(Box::new(fidl_sme::BssInfo{
                        bssid: [0, 0, 0, 0, 0, 0],
                        ssid: network_ssid.as_bytes().to_vec(),
                        rssi_dbm: 0,
                        snr_db: 0,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        protection: fidl_sme::Protection::Unknown,
                        compatible: true,
                        bss_desc: bss_desc,
                    }))
                }).expect("could not send sme response");
            }
        );

        // Send a disconnect request
        let mut client = Client::new(test_values.client_req_sender);
        let (sender, mut receiver) = oneshot::channel();
        client
            .disconnect(types::DisconnectReason::FidlStopClientConnectionsRequest, sender)
            .expect("failed to make request");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Respond to the SME disconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::FidlStopClientConnectionsRequest }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Once the disconnect is processed, the state machine should exit.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Check for a disconnect update and the responder
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::FidlStopClientConnectionsRequest
        );
    }

    #[test]
    fn connected_state_gets_duplicate_connect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let network_ssid = "test";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let initial_state = connected_state(test_values.common_options, connect_request.clone());
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Clear the SME status request
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ responder }) => {
                responder.send(&mut fidl_sme::ClientStatusResponse{
                    connecting_to_ssid: vec![],
                    connected_to: Some(Box::new(fidl_sme::BssInfo{
                        bssid: [0, 0, 0, 0, 0, 0],
                        ssid: network_ssid.as_bytes().to_vec(),
                        rssi_dbm: 0,
                        snr_db: 0,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        protection: fidl_sme::Protection::Unknown,
                        compatible: true,
                        bss_desc,
                    }))
                }).expect("could not send sme response");
            }
        );

        // Send another duplicate request
        let mut client = Client::new(test_values.client_req_sender);
        let (sender, mut receiver) = oneshot::channel();
        client.connect(connect_request.clone(), sender).expect("failed to make request");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure nothing was sent to the SME
        assert_variant!(poll_sme_req(&mut exec, &mut sme_fut), Poll::Pending);

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));

        // No cobalt metrics logged
        validate_no_cobalt_events!(test_values.cobalt_events);
    }

    #[test]
    fn connected_state_gets_different_connect_request() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let first_network_ssid = "foo";
        let second_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: first_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::IdleInterfaceAutoconnect,
        };
        let initial_state = connected_state(test_values.common_options, connect_request.clone());
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Clear the SME status request
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ responder }) => {
                responder.send(&mut fidl_sme::ClientStatusResponse{
                    connecting_to_ssid: vec![],
                    connected_to: Some(Box::new(fidl_sme::BssInfo{
                        bssid: [0, 0, 0, 0, 0, 0],
                        ssid: first_network_ssid.as_bytes().to_vec(),
                        rssi_dbm: 0,
                        snr_db: 0,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        protection: fidl_sme::Protection::Unknown,
                        compatible: true,
                        bss_desc: bss_desc.clone(),
                    }))
                }).expect("could not send sme response");
            }
        );

        // Send a different connect request
        let second_bss_desc = generate_random_bss_desc();
        let mut client = Client::new(test_values.client_req_sender);
        let (connect_sender2, mut connect_receiver2) = oneshot::channel();
        let connect_request2 = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: second_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: second_bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        client.connect(connect_request2.clone(), connect_sender2).expect("failed to make request");

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // There should be 2 requests to the SME stacked up
        // First SME request: disconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::ProactiveNetworkSwitch }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // Progress the state machine
        // TODO(fxbug.dev/53505): remove this once the disconnect request is fire-and-forget
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        // Second SME request: connect to the second network
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, second_network_ssid.as_bytes().to_vec());
                assert_eq!(req.bss_desc, second_bss_desc.clone());
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );
        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(first_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check the responder was acknowledged
        assert_variant!(exec.run_until_stalled(&mut connect_receiver2), Poll::Ready(Ok(())));

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(second_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        // Check for a connected update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(second_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connected,
                status: None,
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure no further updates were sent to listeners
        assert_variant!(
            exec.run_until_stalled(&mut test_values.update_receiver.into_future()),
            Poll::Pending
        );

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::ProactiveNetworkSwitch
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::ProactiveNetworkSwitch
        );
    }

    #[test]
    fn connected_state_detects_network_disconnect() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        // Do test set up manually to get stash server
        set_logger_for_test();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let (update_sender, mut update_receiver) = mpsc::unbounded();
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let sme_req_stream = sme_server.into_stream().expect("could not create SME request stream");
        let temp_dir = tempfile::TempDir::new().expect("failed to create temporary directory");
        let path = temp_dir.path().join(rand_string());
        let tmp_path = temp_dir.path().join(rand_string());
        let (saved_networks, mut stash_server) =
            exec.run_singlethreaded(SavedNetworksManager::new_and_stash_server(path, tmp_path));
        let saved_networks_manager = Arc::new(saved_networks);
        let (cobalt_api, mut cobalt_events) = create_mock_cobalt_sender_and_receiver();
        let network_selector = Arc::new(network_selection::NetworkSelector::new(
            saved_networks_manager.clone(),
            create_mock_cobalt_sender(),
            inspect::Inspector::new().root().create_child("network_selector"),
        ));
        let network_ssid = "foo";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::Password("Anything".as_bytes().to_vec()),
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };

        // Store the network in the saved_networks_manager, so we can record connection success
        let save_fut = saved_networks_manager.store(
            connect_request.target.network.clone().into(),
            connect_request.target.credential.clone(),
        );
        pin_mut!(save_fut);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Pending);
        process_stash_write(&mut exec, &mut stash_server);
        assert_variant!(exec.run_until_stalled(&mut save_fut), Poll::Ready(Ok(None)));

        let common_options = CommonStateOptions {
            proxy: sme_proxy,
            req_stream: client_req_stream.fuse(),
            update_sender: update_sender,
            saved_networks_manager: saved_networks_manager.clone(),
            network_selector,
            cobalt_api: cobalt_api,
        };
        let initial_state = connected_state(common_options, connect_request.clone());
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Respond to the SME status request with a disconnected status
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ responder }) => {
                responder.send(&mut fidl_sme::ClientStatusResponse{
                    connecting_to_ssid: vec![],
                    connected_to: None
                }).expect("could not send sme response");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for an SME disconnect request
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::DisconnectDetectedFromSme }) => {
                responder.send().expect("could not send sme response");
            }
        );
        // Progress the state machine
        // TODO(fxbug.dev/53505): remove this once the disconnect request is fire-and-forget
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionFailed),
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Check for a scan to find a new BSS to reconnect with
        let new_bss_desc = generate_random_bss_desc();
        let expected_scan_request = fidl_sme::ScanRequest::Active(fidl_sme::ActiveScanRequest {
            ssids: vec![network_ssid.as_bytes().to_vec()],
            channels: vec![],
        });
        let mut scan_results = vec![fidl_sme::BssInfo {
            ssid: network_ssid.as_bytes().to_vec(),
            bss_desc: new_bss_desc.clone(),
            compatible: true,
            protection: fidl_sme::Protection::Wpa2Personal,
            ..generate_random_bss_info()
        }];
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Scan {
                txn, req, control_handle: _
            }) => {
                // Validate the request
                assert_eq!(req, expected_scan_request);
                // Send all the APs
                let (_stream, ctrl) = txn
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_result(&mut scan_results.iter_mut())
                    .expect("failed to send scan data");

                // Send the end of data
                ctrl.send_on_finished()
                    .expect("failed to send scan data");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for an SME request to reconnect
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, network_ssid.as_bytes().to_vec());
                assert_eq!(req.bss_desc, new_bss_desc.clone());
                assert_eq!(req.multiple_bss_candidates, false);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Check for a connecting update
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Connecting,
                status: None,
            }],
        };
        assert_variant!(
            update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });

        // Cobalt metrics logged
        validate_cobalt_events!(
            cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::DisconnectDetectedFromSme
        );
        validate_cobalt_events!(
            cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::RetryAfterDisconnectDetected
        );
    }

    #[test]
    fn connected_state_detects_sme_reconnect() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let network_ssid = "foo";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::IdleInterfaceAutoconnect,
        };
        let initial_state = connected_state(test_values.common_options, connect_request.clone());
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Respond to the SME status with `connecting_to_ssid: ssid`
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ responder }) => {
                responder.send(&mut fidl_sme::ClientStatusResponse{
                    connecting_to_ssid: network_ssid.as_bytes().to_vec(),
                    connected_to: None
                }).expect("could not send sme response");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(exec.wake_next_timer(), Some(_)); // wake timer for next SME status request
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Respond to the next SME status request showing we are connected
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ responder }) => {
                responder.send(&mut fidl_sme::ClientStatusResponse{
                    connecting_to_ssid: vec![],
                    connected_to: Some(Box::new(fidl_sme::BssInfo{
                        bssid: [0, 0, 0, 0, 0, 0],
                        ssid: network_ssid.as_bytes().to_vec(),
                        rssi_dbm: 0,
                        snr_db: 0,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        protection: fidl_sme::Protection::Unknown,
                        compatible: true,
                        bss_desc: bss_desc.clone(),
                    }))
                }).expect("could not send sme response");
            }
        );

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(exec.wake_next_timer(), Some(_)); // wake timer for next SME status request
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Next request should be another Status request
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ .. }) => {}
        );

        // Check there were no state updates
        assert_variant!(test_values.update_receiver.try_next(), Err(_));

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::DisconnectDetectedFromSme
        );
        validate_no_cobalt_events!(test_values.cobalt_events);
    }

    #[test]
    fn disconnecting_state_completes_and_exits() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let (sender, mut receiver) = oneshot::channel();
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: Some(sender),
            previous_network: None,
            next_network: None,
            reason: types::DisconnectReason::RegulatoryRegionChange,
        };
        let initial_state = disconnecting_state(test_values.common_options, disconnecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::RegulatoryRegionChange }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Ensure the state machine exits once the disconnect is processed.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Expect the responder to be acknowledged
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::RegulatoryRegionChange
        );
    }

    #[test]
    fn disconnecting_state_completes_disconnect_to_connecting() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());

        let previous_network_ssid = "foo";
        let next_network_ssid = "bar";
        let bss_desc = generate_random_bss_desc();
        let connect_request = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: next_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::ProactiveNetworkSwitch,
        };
        let (connect_sender, _connect_receiver) = oneshot::channel();
        let connecting_options = ConnectingOptions {
            connect_responder: Some(connect_sender),
            connect_request: connect_request.clone(),
            attempt_counter: 0,
        };
        let (disconnect_sender, mut disconnect_receiver) = oneshot::channel();
        // Include both a "previous" and "next" network
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: Some(disconnect_sender),
            previous_network: Some((
                types::NetworkIdentifier {
                    ssid: previous_network_ssid.as_bytes().to_vec(),
                    type_: types::SecurityType::Wpa2,
                },
                fidl_policy::DisconnectStatus::ConnectionStopped,
            )),
            next_network: Some(connecting_options),
            reason: types::DisconnectReason::ProactiveNetworkSwitch,
        };
        let initial_state = disconnecting_state(test_values.common_options, disconnecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Run the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Ensure a disconnect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::ProactiveNetworkSwitch }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Progress the state machine
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Check for a disconnect update and the disconnect responder
        let client_state_update = ClientStateUpdate {
            state: None,
            networks: vec![ClientNetworkState {
                id: fidl_policy::NetworkIdentifier {
                    ssid: String::from(previous_network_ssid).into_bytes(),
                    type_: fidl_policy::SecurityType::Wpa2,
                },
                state: fidl_policy::ConnectionState::Disconnected,
                status: Some(fidl_policy::DisconnectStatus::ConnectionStopped),
            }],
        };
        assert_variant!(
            test_values.update_receiver.try_next(),
            Ok(Some(listener::Message::NotifyListeners(updates))) => {
            assert_eq!(updates, client_state_update);
        });
        assert_variant!(exec.run_until_stalled(&mut disconnect_receiver), Poll::Ready(Ok(())));

        // Ensure a connect request is sent to the SME
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req, txn, control_handle: _ }) => {
                assert_eq!(req.ssid, next_network_ssid.as_bytes().to_vec());
                assert_eq!(req.credential, sme_credential_from_policy(&connect_request.target.credential));
                assert_eq!(req.radio_cfg, RadioConfig { phy: None, cbw: None, primary_chan: None }.to_fidl());
                assert_eq!(req.deprecated_scan_type, fidl_fuchsia_wlan_common::ScanType::Active);
                assert_eq!(req.bss_desc, bss_desc.clone());
                assert_eq!(req.multiple_bss_candidates, true);
                 // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::ProactiveNetworkSwitch
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::ProactiveNetworkSwitch
        );
    }

    #[test]
    fn disconnecting_state_has_broken_sme() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = exec.run_singlethreaded(test_setup());

        let (sender, mut receiver) = oneshot::channel();
        let disconnecting_options = DisconnectingOptions {
            disconnect_responder: Some(sender),
            previous_network: None,
            next_network: None,
            reason: types::DisconnectReason::NetworkConfigUpdated,
        };
        let initial_state = disconnecting_state(test_values.common_options, disconnecting_options);
        let fut = run_state_machine(initial_state);
        pin_mut!(fut);

        // Break the SME by dropping the server end of the SME stream, so it causes an error
        drop(test_values.sme_req_stream);

        // Ensure the state machine exits
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Expect the responder to have an error
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Err(_)));
    }

    #[test]
    fn serve_loop_handles_startup() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());
        let sme_proxy = test_values.common_options.proxy;
        let sme_event_stream = sme_proxy.take_event_stream();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Create a connect request so that the state machine does not immediately exit.
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: "no_password".as_bytes().to_vec(),
                    type_: types::SecurityType::None,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: None,
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::IdleInterfaceAutoconnect,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.cobalt_api,
        );
        pin_mut!(fut);

        // Run the state machine so it sends the initial SME disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::Startup }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Run the future again and ensure that it has not exited after receiving the response.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::Startup
        );
    }

    #[test]
    fn serve_loop_handles_sme_disappearance() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = exec.run_singlethreaded(test_setup());
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);

        // Make our own SME proxy for this test
        let (sme_proxy, sme_server) =
            create_proxy::<fidl_sme::ClientSmeMarker>().expect("failed to create an sme channel");
        let (sme_req_stream, sme_control_handle) = sme_server
            .into_stream_and_control_handle()
            .expect("could not create SME request stream");

        let sme_fut = sme_req_stream.into_future();
        pin_mut!(sme_fut);

        let sme_event_stream = sme_proxy.take_event_stream();

        // Create a connect request so that the state machine does not immediately exit.
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: "no_password".as_bytes().to_vec(),
                    type_: types::SecurityType::None,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: None,
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.cobalt_api,
        );
        pin_mut!(fut);

        // Run the state machine so it sends the initial SME disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::Startup }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Run the future again and ensure that it has not exited after receiving the response.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        sme_control_handle.shutdown_with_epitaph(fuchsia_zircon::Status::UNAVAILABLE);

        // Ensure the state machine has no further actions and is exited
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
    }

    #[test]
    fn serve_loop_handles_disconnect() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let mut test_values = exec.run_singlethreaded(test_setup());
        let sme_proxy = test_values.common_options.proxy;
        let sme_event_stream = sme_proxy.take_event_stream();
        let (client_req_sender, client_req_stream) = mpsc::channel(1);
        let sme_fut = test_values.sme_req_stream.into_future();
        pin_mut!(sme_fut);

        // Create a connect request so that the state machine does not immediately exit.
        let bss_desc = generate_random_bss_desc();
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: "no_password".as_bytes().to_vec(),
                    type_: types::SecurityType::None,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: bss_desc.clone(),
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::RegulatoryChangeReconnect,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.cobalt_api,
        );
        pin_mut!(fut);

        // Run the state machine so it sends the initial SME disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::Startup }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // Run the future again and ensure that it has not exited after receiving the response.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);

        // Absorb the connect request.
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Connect{ req: _, txn, control_handle: _ }) => {
                // Send connection response.
                let (_stream, ctrl) = txn.expect("connect txn unused")
                    .into_stream_and_control_handle().expect("error accessing control handle");
                ctrl.send_on_finished(fidl_sme::ConnectResultCode::Success)
                    .expect("failed to send connection completion");
            }
        );

        // Absorb the initial status request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Status{ responder }) => {
                responder.send(&mut fidl_sme::ClientStatusResponse{
                    connecting_to_ssid: vec![],
                    connected_to: Some(Box::new(fidl_sme::BssInfo{
                        bssid: [0, 0, 0, 0, 0, 0],
                        ssid: "no_password".as_bytes().to_vec(),
                        rssi_dbm: 0,
                        snr_db: 0,
                        channel: fidl_common::WlanChan {
                            primary: 1,
                            cbw: fidl_common::Cbw::Cbw20,
                            secondary80: 0,
                        },
                        protection: fidl_sme::Protection::Unknown,
                        compatible: true,
                        bss_desc,
                    }))
                }).expect("could not send sme response");
            }
        );

        // Send a disconnect request
        let mut client = Client::new(client_req_sender);
        let (sender, mut receiver) = oneshot::channel();
        client
            .disconnect(PolicyDisconnectionMetricDimensionReason::NetworkConfigUpdated, sender)
            .expect("failed to make request");

        // Run the state machine so that it handles the disconnect message.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Pending);
        assert_variant!(
            poll_sme_req(&mut exec, &mut sme_fut),
            Poll::Ready(fidl_sme::ClientSmeRequest::Disconnect{ responder, reason: fidl_sme::UserDisconnectReason::NetworkConfigUpdated }) => {
                responder.send().expect("could not send sme response");
            }
        );

        // The state machine should exit following the disconnect request.
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));

        // Expect the responder to be acknowledged
        assert_variant!(exec.run_until_stalled(&mut receiver), Poll::Ready(Ok(())));

        // Cobalt metrics logged
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::Startup
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            CONNECTION_ATTEMPT_METRIC_ID,
            types::ConnectReason::RegulatoryChangeReconnect
        );
        validate_cobalt_events!(
            test_values.cobalt_events,
            DISCONNECTION_METRIC_ID,
            types::DisconnectReason::NetworkConfigUpdated
        );
    }

    #[test]
    fn serve_loop_handles_state_machine_error() {
        let mut exec = fasync::TestExecutor::new().expect("failed to create an executor");
        let test_values = exec.run_singlethreaded(test_setup());
        let sme_proxy = test_values.common_options.proxy;
        let sme_event_stream = sme_proxy.take_event_stream();
        let (_client_req_sender, client_req_stream) = mpsc::channel(1);

        // Create a connect request so that the state machine does not immediately exit.
        let connect_req = types::ConnectRequest {
            target: types::ConnectionCandidate {
                network: types::NetworkIdentifier {
                    ssid: "no_password".as_bytes().to_vec(),
                    type_: types::SecurityType::None,
                },
                credential: Credential::None,
                observed_in_passive_scan: Some(true),
                bss: None,
                multiple_bss_candidates: Some(true),
            },
            reason: types::ConnectReason::FidlConnectRequest,
        };
        let (sender, _receiver) = oneshot::channel();

        let fut = serve(
            0,
            sme_proxy,
            sme_event_stream,
            client_req_stream,
            test_values.common_options.update_sender,
            test_values.common_options.saved_networks_manager,
            Some((connect_req, sender)),
            test_values.common_options.network_selector,
            test_values.common_options.cobalt_api,
        );
        pin_mut!(fut);

        // Drop the server end of the SME stream, so it causes an error
        drop(test_values.sme_req_stream);

        // Ensure the state machine exits
        assert_variant!(exec.run_until_stalled(&mut fut), Poll::Ready(()));
    }
}
