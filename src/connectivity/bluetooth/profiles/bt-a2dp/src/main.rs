// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "512"]

use {
    anyhow::{format_err, Context as _, Error},
    bt_a2dp::{
        codec::{CodecNegotiation, MediaCodecConfig},
        connected_peers::ConnectedPeers,
        media_types::*,
        peer::ControllerPool,
        stream,
    },
    bt_a2dp_metrics as metrics,
    bt_avdtp::{self as avdtp, ServiceCapability, ServiceCategory, StreamEndpoint},
    fidl_fuchsia_bluetooth_a2dp::{AudioModeRequest, AudioModeRequestStream, Role},
    fidl_fuchsia_bluetooth_bredr as bredr,
    fidl_fuchsia_media::{
        AudioChannelId, AudioPcmMode, PcmFormat, SessionAudioConsumerFactoryMarker,
    },
    fidl_fuchsia_media_sessions2 as sessions2,
    fuchsia_async::{self as fasync, DurationExt},
    fuchsia_bluetooth::{
        assigned_numbers::AssignedNumber,
        profile::{find_profile_descriptors, find_service_classes, profile_descriptor_to_assigned},
        types::{PeerId, Uuid},
    },
    fuchsia_cobalt::{CobaltConnector, CobaltSender, ConnectionType},
    fuchsia_component::server::ServiceFs,
    fuchsia_inspect as inspect,
    fuchsia_inspect_derive::Inspect,
    fuchsia_zircon as zx,
    futures::{self, Stream, StreamExt},
    log::{debug, error, info, trace, warn},
    parking_lot::Mutex,
    profile_client as profile,
    std::{collections::HashSet, convert::TryFrom, sync::Arc},
};

mod avrcp_relay;
mod avrcp_target;
mod config;
mod encoding;
mod latm;
mod pcm_audio;
mod player;
mod sink_task;
mod source_task;
mod sources;
mod volume_relay;

use crate::config::A2dpConfiguration;
use crate::encoding::EncodedStream;
use crate::pcm_audio::PcmAudio;
use sources::AudioSourceType;

/// Make the SDP definition for the A2DP service.
pub(crate) fn make_profile_service_definition(service_uuid: Uuid) -> bredr::ServiceDefinition {
    bredr::ServiceDefinition {
        service_class_uuids: Some(vec![service_uuid.into()]),
        protocol_descriptor_list: Some(vec![
            bredr::ProtocolDescriptor {
                protocol: bredr::ProtocolIdentifier::L2Cap,
                params: vec![bredr::DataElement::Uint16(bredr::PSM_AVDTP)],
            },
            bredr::ProtocolDescriptor {
                protocol: bredr::ProtocolIdentifier::Avdtp,
                params: vec![bredr::DataElement::Uint16(0x0103)], // Indicate v1.3
            },
        ]),
        profile_descriptors: Some(vec![bredr::ProfileDescriptor {
            profile_id: bredr::ServiceClassProfileIdentifier::AdvancedAudioDistribution,
            major_version: 1,
            minor_version: 2,
        }]),
        ..bredr::ServiceDefinition::EMPTY
    }
}

// SDP Attribute ID for the Supported Features of A2DP.
// Defined in Assigned Numbers for SDP
// https://www.bluetooth.com/specifications/assigned-numbers/service-discovery
const ATTR_A2DP_SUPPORTED_FEATURES: u16 = 0x0311;

// Arbitrarily chosen IDs for the endpoints we might enable.
const SBC_SINK_SEID: u8 = 6;
const AAC_SINK_SEID: u8 = 7;
const SBC_SOURCE_SEID: u8 = 8;
const AAC_SOURCE_SEID: u8 = 9;

pub const DEFAULT_SAMPLE_RATE: u32 = 48000;
pub const DEFAULT_SESSION_ID: u64 = 0;

// Highest AAC bitrate we want to transmit
const MAX_BITRATE_AAC: u32 = 320000;

/// Pick a reasonable quality bitrate to use by default. 64k average per channel.
const PREFERRED_BITRATE_AAC: u32 = 128000;

fn find_codec_cap<'a>(endpoint: &'a StreamEndpoint) -> Option<&'a ServiceCapability> {
    endpoint.capabilities().iter().find(|cap| cap.category() == ServiceCategory::MediaCodec)
}

#[derive(Clone)]
struct StreamsBuilder {
    cobalt_sender: CobaltSender,
    codec_negotiation: CodecNegotiation,
    domain: String,
    aac_available: bool,
    sink_enabled: bool,
    source_type: Option<AudioSourceType>,
}

impl StreamsBuilder {
    async fn system_available(
        cobalt_sender: CobaltSender,
        config: &A2dpConfiguration,
    ) -> Result<Self, Error> {
        if !config.enable_sink && !config.enable_source {
            return Err(format_err!("At least one of source or sink must be enabled"));
        }
        // TODO(fxbug.dev/1126): detect codecs, add streams for each codec
        // Sink codecs
        let sbc_endpoint = Self::build_sbc_sink_endpoint()?;
        let sbc_cap = find_codec_cap(&sbc_endpoint).expect("just built");

        // SBC is required to be playable if sink is enabled.
        if config.enable_sink {
            let sbc_config = MediaCodecConfig::try_from(sbc_cap)?;
            if let Err(e) = player::Player::test_playable(&sbc_config).await {
                warn!("Can't play required SBC audio: {}", e);
                return Err(e);
            }
        }

        let aac_available = if config.enable_aac {
            let aac_cap =
                Self::build_aac_capability(avdtp::EndpointType::Sink, /* bitrate=*/ 0)?;
            let aac_config = MediaCodecConfig::try_from(&aac_cap)?;
            if config.enable_sink {
                player::Player::test_playable(&aac_config).await.is_ok()
            } else {
                true
            }
        } else {
            false
        };

        let caps_available = if aac_available {
            vec![
                Self::build_aac_capability(avdtp::EndpointType::Sink, PREFERRED_BITRATE_AAC)?,
                sbc_cap.clone(),
            ]
        } else {
            vec![sbc_cap.clone()]
        };

        let codec_negotiation = CodecNegotiation::build(caps_available, avdtp::EndpointType::Sink)?;
        let source_type = if config.enable_source { Some(config.source) } else { None };

        Ok(Self {
            cobalt_sender,
            codec_negotiation,
            domain: config.domain.clone(),
            aac_available,
            sink_enabled: config.enable_sink,
            source_type,
        })
    }

    const BITPOOL_MAX: u8 = 53; // Maximum recommended bitpool value, from A2DP 1.3.2 Table 4.7

    fn build_sbc_sink_endpoint() -> avdtp::Result<avdtp::StreamEndpoint> {
        let sbc_codec_info = SbcCodecInfo::new(
            SbcSamplingFrequency::MANDATORY_SNK,
            SbcChannelMode::MANDATORY_SNK,
            SbcBlockCount::MANDATORY_SNK,
            SbcSubBands::MANDATORY_SNK,
            SbcAllocation::MANDATORY_SNK,
            SbcCodecInfo::BITPOOL_MIN,
            Self::BITPOOL_MAX,
        )?;
        trace!("Supported SBC codec parameters: {:?}.", sbc_codec_info);

        avdtp::StreamEndpoint::new(
            SBC_SINK_SEID,
            avdtp::MediaType::Audio,
            avdtp::EndpointType::Sink,
            vec![
                ServiceCapability::MediaTransport,
                ServiceCapability::MediaCodec {
                    media_type: avdtp::MediaType::Audio,
                    codec_type: avdtp::MediaCodecType::AUDIO_SBC,
                    codec_extra: sbc_codec_info.to_bytes().to_vec(),
                },
            ],
        )
    }

    fn build_aac_capability(
        endpoint_type: avdtp::EndpointType,
        bitrate: u32,
    ) -> avdtp::Result<avdtp::ServiceCapability> {
        let codec_info = match endpoint_type {
            avdtp::EndpointType::Sink => AacCodecInfo::new(
                AacObjectType::MANDATORY_SNK,
                AacSamplingFrequency::MANDATORY_SNK,
                AacChannels::MANDATORY_SNK,
                true,
                bitrate,
            )?,
            avdtp::EndpointType::Source => AacCodecInfo::new(
                AacObjectType::MANDATORY_SRC,
                AacSamplingFrequency::FREQ48000HZ,
                AacChannels::TWO,
                true,
                bitrate,
            )?,
        };
        trace!("Supported AAC codec parameters: {:?}.", codec_info);
        Ok(ServiceCapability::MediaCodec {
            media_type: avdtp::MediaType::Audio,
            codec_type: avdtp::MediaCodecType::AUDIO_AAC,
            codec_extra: codec_info.to_bytes().to_vec(),
        })
    }

    fn build_aac_sink_endpoint() -> avdtp::Result<avdtp::StreamEndpoint> {
        let endpoint_type = avdtp::EndpointType::Sink;
        // 0 = Unknown constant bitrate support (A2DP Sec. 4.5.2.4)
        let codec_cap = Self::build_aac_capability(endpoint_type, /* bitrate = */ 0)?;
        avdtp::StreamEndpoint::new(
            AAC_SINK_SEID,
            avdtp::MediaType::Audio,
            endpoint_type,
            vec![ServiceCapability::MediaTransport, codec_cap],
        )
    }

    fn build_sbc_source_endpoint() -> avdtp::Result<avdtp::StreamEndpoint> {
        let sbc_codec_info = SbcCodecInfo::new(
            SbcSamplingFrequency::FREQ48000HZ,
            SbcChannelMode::JOINT_STEREO,
            SbcBlockCount::MANDATORY_SRC,
            SbcSubBands::MANDATORY_SRC,
            SbcAllocation::MANDATORY_SRC,
            SbcCodecInfo::BITPOOL_MIN,
            Self::BITPOOL_MAX,
        )?;
        trace!("Supported SBC codec parameters: {:?}.", sbc_codec_info);

        let codec_cap = ServiceCapability::MediaCodec {
            media_type: avdtp::MediaType::Audio,
            codec_type: avdtp::MediaCodecType::AUDIO_SBC,
            codec_extra: sbc_codec_info.to_bytes().to_vec(),
        };

        avdtp::StreamEndpoint::new(
            SBC_SOURCE_SEID,
            avdtp::MediaType::Audio,
            avdtp::EndpointType::Source,
            vec![ServiceCapability::MediaTransport, codec_cap],
        )
    }

    fn build_aac_source_endpoint() -> avdtp::Result<avdtp::StreamEndpoint> {
        let endpoint_type = avdtp::EndpointType::Source;
        let codec_cap = Self::build_aac_capability(endpoint_type, MAX_BITRATE_AAC)?;
        avdtp::StreamEndpoint::new(
            AAC_SOURCE_SEID,
            avdtp::MediaType::Audio,
            endpoint_type,
            vec![ServiceCapability::MediaTransport, codec_cap],
        )
    }

    fn streams(&self) -> Result<stream::Streams, Error> {
        let domain = self.domain.clone();

        let mut streams = stream::Streams::new();

        // Sink streams
        if self.sink_enabled {
            let publisher =
                fuchsia_component::client::connect_to_protocol::<sessions2::PublisherMarker>()
                    .context("Failed to connect to MediaSession interface")?;
            let audio_consumer_factory = fuchsia_component::client::connect_to_protocol::<
                SessionAudioConsumerFactoryMarker,
            >()
            .context("Failed to connect to AudioConsumerFactory")?;
            let sink_task_builder = sink_task::SinkTaskBuilder::new(
                self.cobalt_sender.clone(),
                publisher,
                audio_consumer_factory,
                domain,
            );
            let sbc_sink_endpoint = Self::build_sbc_sink_endpoint()?;
            streams.insert(stream::Stream::build(sbc_sink_endpoint, sink_task_builder.clone()));
            if self.aac_available {
                let aac_sink_endpoint = Self::build_aac_sink_endpoint()?;
                streams.insert(stream::Stream::build(aac_sink_endpoint, sink_task_builder.clone()));
            }
        }

        if let Some(source_type) = self.source_type {
            let source_task_builder = source_task::SourceTaskBuilder::new(source_type);

            let sbc_source_endpoint = Self::build_sbc_source_endpoint()?;
            streams.insert(stream::Stream::build(sbc_source_endpoint, source_task_builder.clone()));

            if self.aac_available {
                let aac_source_endpoint = Self::build_aac_source_endpoint()?;
                streams.insert(stream::Stream::build(
                    aac_source_endpoint,
                    source_task_builder.clone(),
                ));
            }
        }

        Ok(streams)
    }

    fn negotiation(&self) -> CodecNegotiation {
        self.codec_negotiation.clone()
    }
}

/// Establishes the signaling channel after an `initiator_delay`.
async fn connect_after_timeout(
    peer_id: PeerId,
    peers: Arc<Mutex<ConnectedPeers>>,
    channel_mode: bredr::ChannelMode,
    initiator_delay: zx::Duration,
) {
    trace!("waiting {}ms before connecting to peer {}.", initiator_delay.into_millis(), peer_id);
    fuchsia_async::Timer::new(initiator_delay.after_now()).await;

    trace!("{}: trying to connect control channel..", peer_id);
    let connect_fut = peers.lock().try_connect(peer_id.clone(), channel_mode);
    let channel = match connect_fut.await {
        Err(e) => return warn!("Failed to connect control channel to {}: {:?}", peer_id, e),
        Ok(None) => return warn!("Control channel already connected for {}", peer_id),
        Ok(Some(channel)) => channel,
    };

    info!(
        "Connected to {}: mode {} max_tx {}",
        peer_id,
        channel.channel_mode(),
        channel.max_tx_size()
    );
    if let Err(e) = peers.lock().connected(peer_id, channel, Some(zx::Duration::from_nanos(0))) {
        warn!("Problem delivering connection to peer: {}", e);
    }
}

/// Returns the set of supported endpoint directions from a list of service classes.
fn find_endpoint_directions(service_classes: Vec<AssignedNumber>) -> HashSet<avdtp::EndpointType> {
    let mut directions = HashSet::new();
    if service_classes
        .iter()
        .any(|an| an.number == bredr::ServiceClassProfileIdentifier::AudioSource as u16)
    {
        let _ = directions.insert(avdtp::EndpointType::Source);
    }
    if service_classes
        .iter()
        .any(|an| an.number == bredr::ServiceClassProfileIdentifier::AudioSink as u16)
    {
        let _ = directions.insert(avdtp::EndpointType::Sink);
    }
    directions
}

/// Handles found services. Stores the found information and then spawns a task which will
/// assume initiator role after a delay.
fn handle_services_found(
    peer_id: &PeerId,
    attributes: &[bredr::Attribute],
    peers: Arc<Mutex<ConnectedPeers>>,
    channel_mode: bredr::ChannelMode,
    initiator_delay: Option<zx::Duration>,
) {
    let service_classes = find_service_classes(attributes);
    let service_names: Vec<&str> = service_classes.iter().map(|an| an.name).collect();
    let peer_preferred_directions = find_endpoint_directions(service_classes);
    let profiles = find_profile_descriptors(attributes).unwrap_or(vec![]);
    let profile_names: Vec<String> = profiles
        .iter()
        .filter_map(|p| {
            profile_descriptor_to_assigned(p)
                .map(|a| format!("{} ({}.{})", a.name, p.major_version, p.minor_version))
        })
        .collect();
    info!(
        "Audio profile found on {}, classes: {:?}, profiles: {:?}",
        peer_id, service_names, profile_names
    );

    let profile = match profiles.first() {
        Some(profile) => profile.clone(),
        None => {
            info!("Couldn't find profile in peer {} search results, ignoring.", peer_id);
            return;
        }
    };

    debug!("Marking peer {} found...", peer_id);
    peers.lock().found(peer_id.clone(), profile, peer_preferred_directions);

    if let Some(initiator_delay) = initiator_delay {
        fasync::Task::local(connect_after_timeout(
            peer_id.clone(),
            peers.clone(),
            channel_mode,
            initiator_delay,
        ))
        .detach();
    }
}

async fn test_encode_sbc() -> Result<(), Error> {
    // all sinks must support these options
    let required_format = PcmFormat {
        pcm_mode: AudioPcmMode::Linear,
        bits_per_sample: 16,
        frames_per_second: 48000,
        channel_map: vec![AudioChannelId::Lf],
    };
    EncodedStream::test(required_format, &MediaCodecConfig::min_sbc()).await
}

/// Handles role change requests from serving AudioMode
fn handle_audio_mode_connection(
    peers: Arc<Mutex<ConnectedPeers>>,
    mut stream: AudioModeRequestStream,
) {
    fasync::Task::spawn(async move {
        info!("AudioMode Client connected");
        while let Some(request) = stream.next().await {
            match request {
                Err(e) => info!("AudioMode client connection error: {}", e),
                Ok(AudioModeRequest::SetRole { role, responder }) => {
                    // We want to be `role` so we prefer to start streams of the opposite direction.
                    let direction = match role {
                        Role::Source => avdtp::EndpointType::Sink,
                        Role::Sink => avdtp::EndpointType::Source,
                    };
                    info!("Setting AudioMode to {:?}", role);
                    peers.lock().set_preferred_direction(direction);
                    if let Err(e) = responder.send() {
                        warn!("Failed to respond to mode request: {}", e);
                    }
                }
            }
        }
    })
    .detach();
}

fn setup_profiles(
    proxy: bredr::ProfileProxy,
    config: &config::A2dpConfiguration,
) -> Result<profile::ProfileClient, profile::Error> {
    let mut service_defs = Vec::new();
    if config.enable_source {
        let source_uuid = Uuid::new16(bredr::ServiceClassProfileIdentifier::AudioSource as u16);
        service_defs.push(make_profile_service_definition(source_uuid));
    }

    if config.enable_sink {
        let sink_uuid = Uuid::new16(bredr::ServiceClassProfileIdentifier::AudioSink as u16);
        service_defs.push(make_profile_service_definition(sink_uuid));
    }

    let mut profile = profile::ProfileClient::advertise(
        proxy,
        &mut service_defs[..],
        bredr::ChannelParameters {
            channel_mode: Some(config.channel_mode.clone()),
            ..bredr::ChannelParameters::EMPTY
        },
    )?;

    const ATTRS: [u16; 4] = [
        bredr::ATTR_PROTOCOL_DESCRIPTOR_LIST,
        bredr::ATTR_SERVICE_CLASS_ID_LIST,
        bredr::ATTR_BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
        ATTR_A2DP_SUPPORTED_FEATURES,
    ];

    if config.enable_source {
        profile.add_search(bredr::ServiceClassProfileIdentifier::AudioSink, &ATTRS)?;
    }

    if config.enable_sink {
        profile.add_search(bredr::ServiceClassProfileIdentifier::AudioSource, &ATTRS)?;
    }

    Ok(profile)
}

#[fasync::run_singlethreaded]
async fn main() -> Result<(), Error> {
    let config = A2dpConfiguration::load_default()?;

    let initiator_delay =
        if config.initiator_delay.into_millis() == 0 { None } else { Some(config.initiator_delay) };

    fuchsia_syslog::init_with_tags(&["a2dp"]).expect("Can't init logger");
    fuchsia_trace_provider::trace_provider_create_with_fdio();

    // Check to see that we can encode SBC audio.
    // This is a requireement of A2DP 1.3: Section 4.2
    if let Err(e) = test_encode_sbc().await {
        error!("Can't encode SBC Audio: {:?}", e);
        return Ok(());
    }
    let controller_pool = Arc::new(ControllerPool::new());

    let mut fs = ServiceFs::new();

    let inspect = inspect::Inspector::new();
    inspect_runtime::serve(&inspect, &mut fs)?;

    // The absolute volume relay is only needed if A2DP Sink is requested.
    let _abs_vol_relay = if config.enable_sink {
        volume_relay::VolumeRelay::start()
            .or_else(|e| {
                warn!("Failed to start AbsoluteVolume Relay: {:?}", e);
                Err(e)
            })
            .ok()
    } else {
        None
    };

    let cobalt: CobaltSender = {
        let (sender, reporter) =
            CobaltConnector::default().serve(ConnectionType::project_id(metrics::PROJECT_ID));
        fasync::Task::spawn(reporter).detach();
        sender
    };

    let stream_builder = StreamsBuilder::system_available(cobalt.clone(), &config).await?;

    let profile_svc = fuchsia_component::client::connect_to_protocol::<bredr::ProfileMarker>()
        .context("Failed to connect to Bluetooth Profile service")?;

    let mut peers = ConnectedPeers::new(
        stream_builder.streams()?,
        stream_builder.negotiation(),
        1,
        profile_svc.clone(),
        Some(cobalt.clone()),
    );
    if let Err(e) = peers.iattach(&inspect.root(), "connected") {
        warn!("Failed to attach to inspect: {:?}", e);
    }

    let peers_connected_stream = peers.connected_stream();
    let _controller_pool_connected_task = fasync::Task::spawn({
        let pool = controller_pool.clone();
        peers_connected_stream.map(move |p| pool.peer_connected(p)).collect::<()>()
    });

    // The AVRCP Target component is needed if it is requested and A2DP Source is requested.
    let _avrcp_target = if config.enable_source && config.enable_avrcp_target {
        avrcp_target::start_avrcp_target()
            .await
            .or_else(|e| {
                warn!("Couldn't launch AVRCP target: {}", e);
                Err(e)
            })
            .ok()
    } else {
        None
    };

    let peers = Arc::new(Mutex::new(peers));

    fs.dir("svc").add_fidl_service(move |s| controller_pool.connected(s));
    fs.dir("svc").add_fidl_service({
        let peers = peers.clone();
        move |s| handle_audio_mode_connection(peers.clone(), s)
    });
    if let Err(e) = fs.take_and_serve_directory_handle() {
        warn!("Unable to serve service directory: {}", e);
    }
    let _servicefs_task = fasync::Task::spawn(fs.collect::<()>());

    let profile = match setup_profiles(profile_svc.clone(), &config) {
        Err(e) => {
            let err = format!("Failed to setup profiles: {:?}", e);
            error!("{}", err);
            return Err(format_err!("{}", err));
        }
        Ok(profile) => profile,
    };

    handle_profile_events(profile, peers, config.channel_mode, initiator_delay).await
}

async fn handle_profile_events(
    mut profile: impl Stream<Item = Result<profile::ProfileEvent, profile::Error>> + Unpin,
    peers: Arc<Mutex<ConnectedPeers>>,
    channel_mode: bredr::ChannelMode,
    initiator_delay: Option<zx::Duration>,
) -> Result<(), Error> {
    while let Some(item) = profile.next().await {
        let evt = match item {
            Err(e) => return Err(format_err!("Profile client error: {:?}", e)),
            Ok(evt) => evt,
        };
        let peer_id = evt.peer_id().clone();
        match evt {
            profile::ProfileEvent::PeerConnected { channel, .. } => {
                info!(
                    "Connection from {}: mode {} max_tx {}",
                    peer_id,
                    channel.channel_mode(),
                    channel.max_tx_size()
                );
                // Connected, initiate after the delay if not streaming.
                if let Err(e) = peers.lock().connected(peer_id, channel, initiator_delay) {
                    warn!("Problem accepting peer connection: {}", e);
                }
            }
            profile::ProfileEvent::SearchResult { attributes, .. } => {
                handle_services_found(
                    &peer_id,
                    &attributes,
                    peers.clone(),
                    channel_mode.clone(),
                    initiator_delay,
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DEFAULT_INITIATOR_DELAY;

    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_bluetooth_a2dp as a2dp;
    use fidl_fuchsia_bluetooth_bredr::{ProfileRequest, ProfileRequestStream};
    use fidl_fuchsia_cobalt::CobaltEvent;
    use fuchsia_bluetooth::types::Channel;
    use futures::channel::mpsc;
    use futures::{task::Poll, StreamExt};
    use std::{convert::TryInto, iter::FromIterator};

    pub(crate) fn fake_cobalt_sender() -> (CobaltSender, mpsc::Receiver<CobaltEvent>) {
        const BUFFER_SIZE: usize = 100;
        let (sender, receiver) = mpsc::channel(BUFFER_SIZE);
        (CobaltSender::new(sender), receiver)
    }

    fn run_to_stalled(exec: &mut fasync::TestExecutor) {
        let _ = exec.run_until_stalled(&mut futures::future::pending::<()>());
    }

    fn setup_connected_peers() -> (Arc<Mutex<ConnectedPeers>>, ProfileRequestStream) {
        let (proxy, stream) = create_proxy_and_stream::<bredr::ProfileMarker>()
            .expect("Profile proxy should be created");
        let (cobalt_sender, _) = fake_cobalt_sender();
        let peers = Arc::new(Mutex::new(ConnectedPeers::new(
            stream::Streams::new(),
            CodecNegotiation::build(vec![], avdtp::EndpointType::Sink).unwrap(),
            1,
            proxy,
            Some(cobalt_sender),
        )));
        (peers, stream)
    }

    #[test]
    fn test_at_least_one_profile_enabled() {
        let mut exec = fasync::TestExecutor::new().expect("executor should build");
        let (sender, _) = fake_cobalt_sender();
        let config =
            A2dpConfiguration { enable_sink: false, enable_source: false, ..Default::default() };
        let mut streams_fut = Box::pin(StreamsBuilder::system_available(sender, &config));

        let streams = exec.run_singlethreaded(&mut streams_fut);
        assert!(
            streams.is_err(),
            "Stream building should fail when both source and sink are disabled"
        );
    }

    #[cfg(not(feature = "test_encoding"))]
    #[test]
    /// build_local_streams should fail because it can't start the SBC decoder, because
    /// MediaPlayer isn't available in the test environment.
    fn test_sbc_unavailable_error() {
        let mut exec = fasync::TestExecutor::new().expect("executor should build");
        let (sender, _) = fake_cobalt_sender();
        let config = A2dpConfiguration { source: AudioSourceType::BigBen, ..Default::default() };
        let mut streams_fut = Box::pin(StreamsBuilder::system_available(sender, &config));

        let streams = exec.run_singlethreaded(&mut streams_fut);

        assert!(streams.is_err(), "Stream building should fail when it can't reach MediaPlayer");
    }

    #[cfg(feature = "test_encoding")]
    #[test]
    /// build local_streams should not include the AAC streams
    fn test_aac_switch() {
        let mut exec = fasync::TestExecutor::new().expect("executor should build");
        let (sender, _) = fake_cobalt_sender();
        let mut config = A2dpConfiguration {
            source: AudioSourceType::BigBen,
            enable_sink: false,
            ..Default::default()
        };
        let mut builder_fut = Box::pin(StreamsBuilder::system_available(sender.clone(), &config));

        let builder = exec.run_singlethreaded(&mut builder_fut);

        let streams = builder.expect("should generate streams").streams().expect("gather streams");
        assert_eq!(streams.information().len(), 2, "Source AAC & SBC should be available");

        drop(builder_fut);
        drop(streams);

        config.enable_aac = false;

        let mut builder_fut = Box::pin(StreamsBuilder::system_available(sender.clone(), &config));

        let builder = exec.run_singlethreaded(&mut builder_fut);

        let streams = builder.expect("should generate streams").streams().expect("gather streams");
        assert_eq!(streams.information().len(), 1, "Source SBC only should be available");
    }

    #[test]
    /// Tests that A2DP sink assumes the initiator role when a peer is found, but
    /// not connected, and the timeout completes.
    fn wait_to_initiate_success_with_no_connected_peer() {
        let mut exec = fasync::TestExecutor::new_with_fake_time().expect("executor should build");
        let (peers, mut prof_stream) = setup_connected_peers();
        // Initialize context to a fixed point in time.
        exec.set_fake_time(fasync::Time::from_nanos(1000000000));
        let peer_id = PeerId(1);

        // Simulate getting the service found event.
        let attributes = vec![bredr::Attribute {
            id: bredr::ATTR_BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            element: bredr::DataElement::Sequence(vec![Some(Box::new(
                bredr::DataElement::Sequence(vec![
                    Some(Box::new(
                        Uuid::from(bredr::ServiceClassProfileIdentifier::AudioSource).into(),
                    )),
                    Some(Box::new(bredr::DataElement::Uint16(0x0103))), // Version 1.3
                ]),
            ))]),
        }];
        handle_services_found(
            &peer_id,
            &attributes,
            peers.clone(),
            bredr::ChannelMode::Basic,
            Some(DEFAULT_INITIATOR_DELAY),
        );

        run_to_stalled(&mut exec);

        // At this point, a remote peer was found, but hasn't connected yet. There
        // should be no entry for it.
        assert!(!peers.lock().is_connected(&peer_id));

        // Fast forward time by 5 seconds. In this time, the remote peer has not
        // connected.
        exec.set_fake_time(fasync::Time::from_nanos(6000000000));
        exec.wake_expired_timers();
        run_to_stalled(&mut exec);

        // After fast forwarding time, expect and handle the `connect` request
        // because A2DP-sink should be initiating.
        let (_test, transport) = Channel::create();
        let request = exec.run_until_stalled(&mut prof_stream.next());
        match request {
            Poll::Ready(Some(Ok(ProfileRequest::Connect { peer_id, responder, .. }))) => {
                assert_eq!(PeerId(1), peer_id.into());
                let channel = transport.try_into().unwrap();
                responder.send(&mut Ok(channel)).expect("responder sends");
            }
            x => panic!("Should have sent a connect request, but got {:?}", x),
        };
        run_to_stalled(&mut exec);

        // The remote peer did not connect to us, A2DP Sink should initiate a connection
        // and insert into `peers`.
        assert!(peers.lock().is_connected(&peer_id));
    }

    #[test]
    /// Tests that A2DP sink does not assume the initiator role when a peer connects
    /// before `INITIATOR_DELAY` timeout completes.
    fn wait_to_initiate_returns_early_with_connected_peer() {
        let mut exec = fasync::TestExecutor::new_with_fake_time().expect("executor should build");
        let (peers, mut prof_stream) = setup_connected_peers();
        // Initialize context to a fixed point in time.
        exec.set_fake_time(fasync::Time::from_nanos(1000000000));
        let peer_id = PeerId(1);

        // Simulate getting the service found event.
        let attributes = vec![bredr::Attribute {
            id: bredr::ATTR_BLUETOOTH_PROFILE_DESCRIPTOR_LIST,
            element: bredr::DataElement::Sequence(vec![Some(Box::new(
                bredr::DataElement::Sequence(vec![
                    Some(Box::new(
                        Uuid::from(bredr::ServiceClassProfileIdentifier::AudioSource).into(),
                    )),
                    Some(Box::new(bredr::DataElement::Uint16(0x0103))), // Version 1.3
                ]),
            ))]),
        }];
        handle_services_found(
            &peer_id,
            &attributes,
            peers.clone(),
            bredr::ChannelMode::Basic,
            Some(DEFAULT_INITIATOR_DELAY),
        );

        // At this point, a remote peer was found, but hasn't connected yet. There
        // should be no entry for it.
        assert!(!peers.lock().is_connected(&peer_id));

        // Fast forward time by .5 seconds. The threshold is 1 second, so the timer
        // to initiate connections has not been triggered.
        exec.set_fake_time(fasync::Time::after(zx::Duration::from_millis(500)));
        exec.wake_expired_timers();
        run_to_stalled(&mut exec);

        // A peer connects before the timeout.
        let (_remote, signaling) = Channel::create();
        let _ = peers.lock().connected(peer_id.clone(), signaling, None);

        run_to_stalled(&mut exec);

        // The remote peer connected to us, and should be in the map.
        assert!(peers.lock().is_connected(&peer_id));

        // Fast forward time by 4.5 seconds. Ensure no outbound connection is initiated
        // by us, since the remote peer has assumed the INT role.
        exec.set_fake_time(fasync::Time::after(zx::Duration::from_millis(4500)));
        exec.wake_expired_timers();
        run_to_stalled(&mut exec);

        let request = exec.run_until_stalled(&mut prof_stream.next());
        match request {
            Poll::Ready(x) => panic!("There should be no l2cap connection requests: {:?}", x),
            Poll::Pending => {}
        };
        run_to_stalled(&mut exec);
    }

    #[cfg(not(feature = "test_encoding"))]
    #[test]
    fn test_encoding_fails_in_test_environment() {
        let mut exec = fasync::TestExecutor::new().expect("executor should build");
        let result = exec.run_singlethreaded(test_encode_sbc());

        assert!(result.is_err());
    }

    #[test]
    fn test_audio_mode_connection() {
        let mut exec = fasync::TestExecutor::new().expect("executor should build");
        let (peers, _profile_stream) = setup_connected_peers();

        let (proxy, stream) = create_proxy_and_stream::<a2dp::AudioModeMarker>()
            .expect("AudioMode proxy should be created");

        handle_audio_mode_connection(peers.clone(), stream);

        exec.run_singlethreaded(proxy.set_role(a2dp::Role::Sink)).expect("set role response");

        assert_eq!(avdtp::EndpointType::Source, peers.lock().preferred_direction());

        exec.run_singlethreaded(proxy.set_role(a2dp::Role::Source)).expect("set role response");

        assert_eq!(avdtp::EndpointType::Sink, peers.lock().preferred_direction());
    }

    #[test]
    fn find_endpoint_directions_returns_expected_direction() {
        let empty = Vec::new();
        assert_eq!(find_endpoint_directions(empty), HashSet::new());

        let no_a2dp_attributes =
            vec![AssignedNumber { number: 0x1234, abbreviation: None, name: "FooBar" }];
        assert_eq!(find_endpoint_directions(no_a2dp_attributes), HashSet::new());

        let sink_attribute = AssignedNumber {
            number: bredr::ServiceClassProfileIdentifier::AudioSink as u16,
            abbreviation: None,
            name: "AudioSink",
        };
        let source_attribute = AssignedNumber {
            number: bredr::ServiceClassProfileIdentifier::AudioSource as u16,
            abbreviation: None,
            name: "AudioSource",
        };

        let only_sink = vec![sink_attribute.clone()];
        let expected_directions = HashSet::from_iter(vec![avdtp::EndpointType::Sink].into_iter());
        assert_eq!(find_endpoint_directions(only_sink), expected_directions);

        let only_source = vec![source_attribute.clone()];
        let expected_directions = HashSet::from_iter(vec![avdtp::EndpointType::Source].into_iter());
        assert_eq!(find_endpoint_directions(only_source), expected_directions);

        let both = vec![sink_attribute, source_attribute];
        let expected_directions = HashSet::from_iter(
            vec![avdtp::EndpointType::Sink, avdtp::EndpointType::Source].into_iter(),
        );
        assert_eq!(find_endpoint_directions(both), expected_directions);
    }
}
