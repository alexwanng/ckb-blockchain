use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ckb_logger::{debug, error, trace, warn};
use p2p::{
    bytes::Bytes,
    context::{ProtocolContext, ProtocolContextMutRef, SessionContext},
    multiaddr::{Multiaddr, Protocol},
    secio::{PeerId, PublicKey},
    service::{SessionType, TargetProtocol},
    traits::ServiceProtocol,
    utils::{is_reachable, multiaddr_to_socketaddr},
    SessionId,
};

mod protocol;

use crate::{network::FEELER_PROTOCOL_ID, NetworkState, PeerIdentifyInfo};
use ckb_types::{packed, prelude::*};

use protocol::IdentifyMessage;

const MAX_RETURN_LISTEN_ADDRS: usize = 10;
const BAN_ON_NOT_SAME_NET: Duration = Duration::from_secs(5 * 60);
const CHECK_TIMEOUT_TOKEN: u64 = 100;
// Check timeout interval (seconds)
const CHECK_TIMEOUT_INTERVAL: u64 = 1;
const DEFAULT_TIMEOUT: u64 = 8;
const MAX_ADDRS: usize = 10;

/// The misbehavior to report to underlying peer storage
pub enum Misbehavior {
    /// Repeat send listen addresses
    DuplicateListenAddrs,
    /// Repeat send observed address
    DuplicateObservedAddr,
    /// Timeout reached
    Timeout,
    /// Remote peer send invalid data
    InvalidData,
    /// Send too many addresses in listen addresses
    TooManyAddresses(usize),
}

/// Misbehavior report result
pub enum MisbehaveResult {
    /// Continue to run
    Continue,
    /// Disconnect this peer
    Disconnect,
}

impl MisbehaveResult {
    pub fn is_disconnect(&self) -> bool {
        match self {
            MisbehaveResult::Disconnect => true,
            _ => false,
        }
    }
}

/// The trait to communicate with underlying peer storage
pub trait Callback: Clone + Send {
    /// Received custom message
    fn received_identify(
        &mut self,
        context: &mut ProtocolContextMutRef,
        identify: &[u8],
    ) -> MisbehaveResult;
    /// Get custom identify message
    fn identify(&mut self) -> &[u8];
    /// Get local listen addresses
    fn local_listen_addrs(&mut self) -> Vec<Multiaddr>;
    /// Add remote peer's listen addresses
    fn add_remote_listen_addrs(&mut self, peer: &PeerId, addrs: Vec<Multiaddr>);
    /// Add our address observed by remote peer
    fn add_observed_addr(
        &mut self,
        peer: &PeerId,
        addr: Multiaddr,
        ty: SessionType,
    ) -> MisbehaveResult;
    /// Report misbehavior
    fn misbehave(&mut self, peer: &PeerId, kind: Misbehavior) -> MisbehaveResult;
}

/// Identify protocol
pub struct IdentifyProtocol<T> {
    callback: T,
    remote_infos: HashMap<SessionId, RemoteInfo>,
    secio_enabled: bool,
    global_ip_only: bool,
}

impl<T: Callback> IdentifyProtocol<T> {
    pub fn new(callback: T) -> IdentifyProtocol<T> {
        IdentifyProtocol {
            callback,
            remote_infos: HashMap::default(),
            secio_enabled: true,
            global_ip_only: true,
        }
    }

    /// Turning off global ip only mode will allow any ip to be broadcast, default is true
    // pub fn global_ip_only(mut self, global_ip_only: bool) -> Self {
    //     self.global_ip_only = global_ip_only;
    //     self
    // }

    fn process_listens(
        &mut self,
        context: &mut ProtocolContextMutRef,
        listens: Vec<Multiaddr>,
    ) -> MisbehaveResult {
        let session = context.session;
        let info = self
            .remote_infos
            .get_mut(&session.id)
            .expect("RemoteInfo must exists");

        if info.listen_addrs.is_some() {
            debug!("remote({:?}) repeat send observed address", info.peer_id);
            self.callback
                .misbehave(&info.peer_id, Misbehavior::DuplicateListenAddrs)
        } else if listens.len() > MAX_ADDRS {
            self.callback
                .misbehave(&info.peer_id, Misbehavior::TooManyAddresses(listens.len()))
        } else {
            trace!("received listen addresses: {:?}", listens);
            let global_ip_only = self.global_ip_only;
            let reachable_addrs = listens
                .into_iter()
                .filter(|addr| {
                    multiaddr_to_socketaddr(addr)
                        .map(|socket_addr| !global_ip_only || is_reachable(socket_addr.ip()))
                        .unwrap_or(false)
                })
                .collect::<Vec<_>>();
            self.callback
                .add_remote_listen_addrs(&info.peer_id, reachable_addrs.clone());
            info.listen_addrs = Some(reachable_addrs);
            MisbehaveResult::Continue
        }
    }

    fn process_observed(
        &mut self,
        context: &mut ProtocolContextMutRef,
        observed: Multiaddr,
    ) -> MisbehaveResult {
        let session = context.session;
        let mut info = self
            .remote_infos
            .get_mut(&session.id)
            .expect("RemoteInfo must exists");

        if info.observed_addr.is_some() {
            debug!("remote({:?}) repeat send listen addresses", info.peer_id);
            self.callback
                .misbehave(&info.peer_id, Misbehavior::DuplicateObservedAddr)
        } else {
            trace!("received observed address: {}", observed);

            let global_ip_only = self.global_ip_only;
            if multiaddr_to_socketaddr(&observed)
                .map(|socket_addr| socket_addr.ip())
                .filter(|ip_addr| !global_ip_only || is_reachable(*ip_addr))
                .is_some()
                && self
                    .callback
                    .add_observed_addr(&info.peer_id, observed.clone(), info.session.ty)
                    .is_disconnect()
            {
                return MisbehaveResult::Disconnect;
            }
            info.observed_addr = Some(observed);
            MisbehaveResult::Continue
        }
    }
}

pub(crate) struct RemoteInfo {
    peer_id: PeerId,
    session: SessionContext,
    connected_at: Instant,
    timeout: Duration,
    listen_addrs: Option<Vec<Multiaddr>>,
    observed_addr: Option<Multiaddr>,
}

impl RemoteInfo {
    fn new(session: SessionContext, timeout: Duration) -> RemoteInfo {
        let peer_id = session
            .remote_pubkey
            .as_ref()
            .map(|key| PeerId::from_public_key(&key))
            .expect("secio must enabled!");
        RemoteInfo {
            peer_id,
            session,
            connected_at: Instant::now(),
            timeout,
            listen_addrs: None,
            observed_addr: None,
        }
    }
}

impl<T: Callback> ServiceProtocol for IdentifyProtocol<T> {
    fn init(&mut self, context: &mut ProtocolContext) {
        let proto_id = context.proto_id;
        if context
            .set_service_notify(
                proto_id,
                Duration::from_secs(CHECK_TIMEOUT_INTERVAL),
                CHECK_TIMEOUT_TOKEN,
            )
            .is_err()
        {
            warn!("identify start fail")
        }
    }

    fn connected(&mut self, context: ProtocolContextMutRef, _version: &str) {
        let session = context.session;
        if session.remote_pubkey.is_none() {
            error!("IdentifyProtocol require secio enabled!");
            let _ = context.disconnect(session.id);
            self.secio_enabled = false;
            return;
        }

        let remote_info = RemoteInfo::new(session.clone(), Duration::from_secs(DEFAULT_TIMEOUT));
        trace!("IdentifyProtocol sconnected from {:?}", remote_info.peer_id);
        self.remote_infos.insert(session.id, remote_info);

        let listen_addrs: Vec<Multiaddr> = self
            .callback
            .local_listen_addrs()
            .iter()
            .filter(|addr| {
                multiaddr_to_socketaddr(addr)
                    .map(|socket_addr| !self.global_ip_only || is_reachable(socket_addr.ip()))
                    .unwrap_or(false)
            })
            .take(MAX_ADDRS)
            .cloned()
            .collect();

        let observed_addr = session
            .address
            .iter()
            .filter(|proto| match proto {
                Protocol::P2P(_) => false,
                _ => true,
            })
            .collect::<Multiaddr>();

        let identify = self.callback.identify();
        let data = IdentifyMessage::new(listen_addrs, observed_addr, identify).encode();
        let _ = context.quick_send_message(data);
    }

    fn disconnected(&mut self, context: ProtocolContextMutRef) {
        if self.secio_enabled {
            let info = self
                .remote_infos
                .remove(&context.session.id)
                .expect("RemoteInfo must exists");
            trace!("IdentifyProtocol disconnected from {:?}", info.peer_id);
        }
    }

    fn received(&mut self, mut context: ProtocolContextMutRef, data: Bytes) {
        if !self.secio_enabled {
            return;
        }

        let session = context.session;

        match IdentifyMessage::decode(&data) {
            Some(message) => {
                // Need to interrupt processing, avoid pollution
                if self
                    .callback
                    .received_identify(&mut context, message.identify)
                    .is_disconnect()
                    || self
                        .process_listens(&mut context, message.listen_addrs)
                        .is_disconnect()
                    || self
                        .process_observed(&mut context, message.observed_addr)
                        .is_disconnect()
                {
                    let _ = context.disconnect(session.id);
                }
            }
            None => {
                let info = self
                    .remote_infos
                    .get(&session.id)
                    .expect("RemoteInfo must exists");
                debug!(
                    "IdentifyProtocol received invalid data from {:?}",
                    info.peer_id
                );
                if self
                    .callback
                    .misbehave(&info.peer_id, Misbehavior::InvalidData)
                    .is_disconnect()
                {
                    let _ = context.disconnect(session.id);
                }
            }
        }
    }

    fn notify(&mut self, context: &mut ProtocolContext, _token: u64) {
        if !self.secio_enabled {
            return;
        }

        let now = Instant::now();
        for (session_id, info) in &self.remote_infos {
            if (info.listen_addrs.is_none() || info.observed_addr.is_none())
                && (info.connected_at + info.timeout) <= now
            {
                debug!("{:?} receive identify message timeout", info.peer_id);
                if self
                    .callback
                    .misbehave(&info.peer_id, Misbehavior::Timeout)
                    .is_disconnect()
                {
                    let _ = context.disconnect(*session_id);
                }
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct IdentifyCallback {
    network_state: Arc<NetworkState>,
    identify: Identify,
}

impl IdentifyCallback {
    pub(crate) fn new(
        network_state: Arc<NetworkState>,
        name: String,
        client_version: String,
    ) -> IdentifyCallback {
        let flags = Flags(Flag::FullNode as u64);

        IdentifyCallback {
            network_state,
            identify: Identify::new(name, flags, client_version),
        }
    }

    fn listen_addrs(&self) -> Vec<Multiaddr> {
        let mut addrs = self.network_state.public_addrs(MAX_RETURN_LISTEN_ADDRS * 2);
        addrs.sort_by(|a, b| a.1.cmp(&b.1));
        addrs
            .into_iter()
            .take(MAX_RETURN_LISTEN_ADDRS)
            .map(|(addr, _)| addr)
            .collect::<Vec<_>>()
    }
}

impl Callback for IdentifyCallback {
    fn identify(&mut self) -> &[u8] {
        self.identify.encode()
    }

    fn received_identify(
        &mut self,
        context: &mut ProtocolContextMutRef,
        identify: &[u8],
    ) -> MisbehaveResult {
        match self.identify.verify(identify) {
            None => {
                self.network_state.ban_session(
                    context.control(),
                    context.session.id,
                    BAN_ON_NOT_SAME_NET,
                    "The nodes are not on the same network".to_string(),
                );
                MisbehaveResult::Disconnect
            }
            Some((flags, client_version)) => {
                let registry_client_version = |version: String| {
                    self.network_state.with_peer_registry_mut(|registry| {
                        if let Some(peer) = registry.get_peer_mut(context.session.id) {
                            peer.identify_info = Some(PeerIdentifyInfo {
                                client_version: version,
                            })
                        }
                    });
                };

                if context.session.ty.is_outbound() {
                    let peer_id = context
                        .session
                        .remote_pubkey
                        .as_ref()
                        .map(PublicKey::peer_id)
                        .expect("Secio must enabled");
                    if self
                        .network_state
                        .with_peer_registry(|reg| reg.is_feeler(&peer_id))
                    {
                        let _ = context.open_protocols(
                            context.session.id,
                            TargetProtocol::Single(FEELER_PROTOCOL_ID.into()),
                        );
                    } else if flags.contains(self.identify.flags) {
                        registry_client_version(client_version);

                        // The remote end can support all local protocols.
                        let protos = self
                            .network_state
                            .get_protocol_ids(|id| id != FEELER_PROTOCOL_ID.into());

                        let _ = context
                            .open_protocols(context.session.id, TargetProtocol::Multi(protos));
                    } else {
                        // The remote end cannot support all local protocols.
                        return MisbehaveResult::Disconnect;
                    }
                } else {
                    registry_client_version(client_version);
                }
                MisbehaveResult::Continue
            }
        }
    }

    /// Get local listen addresses
    fn local_listen_addrs(&mut self) -> Vec<Multiaddr> {
        self.listen_addrs()
    }

    fn add_remote_listen_addrs(&mut self, peer_id: &PeerId, addrs: Vec<Multiaddr>) {
        trace!(
            "got remote listen addrs from peer_id={:?}, addrs={:?}",
            peer_id,
            addrs,
        );
        self.network_state.with_peer_registry_mut(|reg| {
            if let Some(peer) = reg
                .get_key_by_peer_id(peer_id)
                .and_then(|session_id| reg.get_peer_mut(session_id))
            {
                peer.listened_addrs = addrs.clone();
            }
        });
        self.network_state.with_peer_store_mut(|peer_store| {
            for addr in addrs {
                if let Err(err) = peer_store.add_addr(peer_id.clone(), addr) {
                    debug!("Failed to add addrs to peer_store {:?} {:?}", err, peer_id);
                }
            }
        })
    }

    fn add_observed_addr(
        &mut self,
        peer_id: &PeerId,
        addr: Multiaddr,
        ty: SessionType,
    ) -> MisbehaveResult {
        debug!(
            "peer({:?}, {:?}) reported observed addr {}",
            peer_id, ty, addr,
        );

        if ty.is_inbound() {
            // The address already been discovered by other peer
            return MisbehaveResult::Continue;
        }

        // observed addr is not a reachable ip
        if !multiaddr_to_socketaddr(&addr)
            .map(|socket_addr| is_reachable(socket_addr.ip()))
            .unwrap_or(false)
        {
            return MisbehaveResult::Continue;
        }

        let observed_addrs_iter = self
            .listen_addrs()
            .into_iter()
            .filter_map(|listen_addr| multiaddr_to_socketaddr(&listen_addr))
            .map(|socket_addr| {
                addr.iter()
                    .filter_map(|proto| match proto {
                        Protocol::P2P(_) => None,
                        Protocol::TCP(_) => Some(Protocol::TCP(socket_addr.port())),
                        value => Some(value),
                    })
                    .collect::<Multiaddr>()
            });
        self.network_state.add_observed_addrs(observed_addrs_iter);
        // NOTE: for future usage
        MisbehaveResult::Continue
    }

    fn misbehave(&mut self, _peer_id: &PeerId, _kind: Misbehavior) -> MisbehaveResult {
        MisbehaveResult::Disconnect
    }
}

#[derive(Clone)]
struct Identify {
    name: String,
    client_version: String,
    flags: Flags,
    encode_data: ckb_types::bytes::Bytes,
}

impl Identify {
    fn new(name: String, flags: Flags, client_version: String) -> Self {
        Identify {
            name,
            client_version,
            flags,
            encode_data: ckb_types::bytes::Bytes::default(),
        }
    }

    fn encode(&mut self) -> &[u8] {
        if self.encode_data.is_empty() {
            self.encode_data = packed::Identify::new_builder()
                .name(self.name.as_str().pack())
                .flag(self.flags.0.pack())
                .client_version(self.client_version.as_str().pack())
                .build()
                .as_bytes();
        }

        &self.encode_data
    }

    fn verify<'a>(&self, data: &'a [u8]) -> Option<(Flags, String)> {
        let reader = packed::IdentifyReader::from_slice(data).ok()?;

        let name = reader.name().as_utf8().ok()?.to_owned();
        if self.name != name {
            debug!("Not the same chain, self: {}, remote: {}", self.name, name);
            return None;
        }

        let flag: u64 = reader.flag().unpack();
        if flag == 0 {
            return None;
        }

        let raw_client_version = reader.client_version().as_utf8().ok()?.to_owned();

        Some((Flags::from(flag), raw_client_version))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u64)]
enum Flag {
    /// Support all protocol
    FullNode = 0x1,
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct Flags(u64);

impl Flags {
    /// Check if contains a target flag
    fn contains(self, flags: Flags) -> bool {
        (self.0 & flags.0) == flags.0
    }
}

impl From<Flag> for Flags {
    fn from(value: Flag) -> Flags {
        Flags(value as u64)
    }
}

impl From<u64> for Flags {
    fn from(value: u64) -> Flags {
        Flags(value)
    }
}
