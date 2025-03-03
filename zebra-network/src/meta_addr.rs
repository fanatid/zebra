//! An address-with-metadata type used in Bitcoin networking.

use std::{
    cmp::{Ord, Ordering},
    io::{Read, Write},
    net::SocketAddr,
};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use chrono::{DateTime, TimeZone, Utc};

use zebra_chain::serialization::{
    ReadZcashExt, SerializationError, TrustedPreallocate, WriteZcashExt, ZcashDeserialize,
    ZcashSerialize,
};

use crate::protocol::{external::MAX_PROTOCOL_MESSAGE_LEN, types::PeerServices};

use PeerAddrState::*;

#[cfg(any(test, feature = "proptest-impl"))]
use proptest_derive::Arbitrary;
#[cfg(any(test, feature = "proptest-impl"))]
mod arbitrary;

#[cfg(test)]
mod tests;

/// Peer connection state, based on our interactions with the peer.
///
/// Zebra also tracks how recently a peer has sent us messages, and derives peer
/// liveness based on the current time. This derived state is tracked using
/// [`AddressBook::maybe_connected_peers`] and
/// [`AddressBook::reconnection_peers`].
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[cfg_attr(any(test, feature = "proptest-impl"), derive(Arbitrary))]
pub enum PeerAddrState {
    /// The peer has sent us a valid message.
    ///
    /// Peers remain in this state, even if they stop responding to requests.
    /// (Peer liveness is derived from the `last_seen` timestamp, and the current
    /// time.)
    Responded,

    /// The peer's address has just been fetched from a DNS seeder, or via peer
    /// gossip, but we haven't attempted to connect to it yet.
    NeverAttempted,

    /// The peer's TCP connection failed, or the peer sent us an unexpected
    /// Zcash protocol message, so we failed the connection.
    Failed,

    /// We just started a connection attempt to this peer.
    AttemptPending,
}

impl Default for PeerAddrState {
    fn default() -> Self {
        NeverAttempted
    }
}

impl Ord for PeerAddrState {
    /// `PeerAddrState`s are sorted in approximate reconnection attempt
    /// order, ignoring liveness.
    ///
    /// See [`CandidateSet`] and [`MetaAddr::cmp`] for more details.
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Responded, Responded)
            | (NeverAttempted, NeverAttempted)
            | (Failed, Failed)
            | (AttemptPending, AttemptPending) => Ordering::Equal,
            // We reconnect to `Responded` peers that have stopped sending messages,
            // then `NeverAttempted` peers, then `Failed` peers
            (Responded, _) => Ordering::Less,
            (_, Responded) => Ordering::Greater,
            (NeverAttempted, _) => Ordering::Less,
            (_, NeverAttempted) => Ordering::Greater,
            (Failed, _) => Ordering::Less,
            (_, Failed) => Ordering::Greater,
            // AttemptPending is covered by the other cases
        }
    }
}

impl PartialOrd for PeerAddrState {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// An address with metadata on its advertised services and last-seen time.
///
/// [Bitcoin reference](https://en.bitcoin.it/wiki/Protocol_documentation#Network_address)
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct MetaAddr {
    /// The peer's address.
    pub addr: SocketAddr,

    /// The services advertised by the peer.
    ///
    /// The exact meaning depends on `last_connection_state`:
    ///   - `Responded`: the services advertised by this peer, the last time we
    ///      performed a handshake with it
    ///   - `NeverAttempted`: the unverified services provided by the remote peer
    ///     that sent us this address
    ///   - `Failed` or `AttemptPending`: unverified services via another peer,
    ///      or services advertised in a previous handshake
    ///
    /// ## Security
    ///
    /// `services` from `NeverAttempted` peers may be invalid due to outdated
    /// records, older peer versions, or buggy or malicious peers.
    pub services: PeerServices,

    /// The last time we interacted with this peer.
    ///
    /// See `get_last_seen` for details.
    last_seen: DateTime<Utc>,

    /// The outcome of our most recent communication attempt with this peer.
    pub last_connection_state: PeerAddrState,
}

impl MetaAddr {
    /// Create a new `MetaAddr` from the deserialized fields in an `Addr`
    /// message.
    pub fn new_gossiped(
        addr: &SocketAddr,
        services: &PeerServices,
        last_seen: &DateTime<Utc>,
    ) -> MetaAddr {
        MetaAddr {
            addr: *addr,
            services: *services,
            last_seen: *last_seen,
            // the state is Zebra-specific, it isn't part of the Zcash network protocol
            last_connection_state: NeverAttempted,
        }
    }

    /// Create a new `MetaAddr` for a peer that has just `Responded`.
    pub fn new_responded(addr: &SocketAddr, services: &PeerServices) -> MetaAddr {
        MetaAddr {
            addr: *addr,
            services: *services,
            last_seen: Utc::now(),
            last_connection_state: Responded,
        }
    }

    /// Create a new `MetaAddr` for a peer that we want to reconnect to.
    pub fn new_reconnect(addr: &SocketAddr, services: &PeerServices) -> MetaAddr {
        MetaAddr {
            addr: *addr,
            services: *services,
            last_seen: Utc::now(),
            last_connection_state: AttemptPending,
        }
    }

    /// Create a new `MetaAddr` for a peer that has just had an error.
    pub fn new_errored(addr: &SocketAddr, services: &PeerServices) -> MetaAddr {
        MetaAddr {
            addr: *addr,
            services: *services,
            last_seen: Utc::now(),
            last_connection_state: Failed,
        }
    }

    /// Create a new `MetaAddr` for a peer that has just shut down.
    pub fn new_shutdown(addr: &SocketAddr, services: &PeerServices) -> MetaAddr {
        // TODO: if the peer shut down in the Responded state, preserve that
        // state. All other states should be treated as (timeout) errors.
        MetaAddr::new_errored(addr, services)
    }

    /// The last time we interacted with this peer.
    ///
    /// The exact meaning depends on `last_connection_state`:
    ///   - `Responded`: the last time we processed a message from this peer
    ///   - `NeverAttempted`: the unverified time provided by the remote peer
    ///     that sent us this address
    ///   - `Failed`: the last time we marked the peer as failed
    ///   - `AttemptPending`: the last time we queued the peer for a reconnection
    ///     attempt
    ///
    /// ## Security
    ///
    /// `last_seen` times from `NeverAttempted` peers may be invalid due to
    /// clock skew, or buggy or malicious peers.
    pub fn get_last_seen(&self) -> DateTime<Utc> {
        self.last_seen
    }

    /// Return a sanitized version of this `MetaAddr`, for sending to a remote peer.
    pub fn sanitize(&self) -> MetaAddr {
        let interval = crate::constants::TIMESTAMP_TRUNCATION_SECONDS;
        let ts = self.get_last_seen().timestamp();
        let last_seen = Utc.timestamp(ts - ts.rem_euclid(interval), 0);
        MetaAddr {
            addr: self.addr,
            // services are sanitized during parsing, so we don't need to make
            // any changes here
            services: self.services,
            last_seen,
            // the state isn't sent to the remote peer, but sanitize it anyway
            last_connection_state: Default::default(),
        }
    }
}

impl Ord for MetaAddr {
    /// `MetaAddr`s are sorted in approximate reconnection attempt order, but
    /// with `Responded` peers sorted first as a group.
    ///
    /// This order should not be used for reconnection attempts: use
    /// [`AddressBook::reconnection_peers`] instead.
    ///
    /// See [`CandidateSet`] for more details.
    fn cmp(&self, other: &Self) -> Ordering {
        use std::net::IpAddr::{V4, V6};

        let oldest_first = self.get_last_seen().cmp(&other.get_last_seen());
        let newest_first = oldest_first.reverse();

        let connection_state = self.last_connection_state.cmp(&other.last_connection_state);
        let reconnection_time = match self.last_connection_state {
            Responded => oldest_first,
            NeverAttempted => newest_first,
            Failed => oldest_first,
            AttemptPending => oldest_first,
        };
        let ip_numeric = match (self.addr.ip(), other.addr.ip()) {
            (V4(a), V4(b)) => a.octets().cmp(&b.octets()),
            (V6(a), V6(b)) => a.octets().cmp(&b.octets()),
            (V4(_), V6(_)) => Ordering::Less,
            (V6(_), V4(_)) => Ordering::Greater,
        };

        connection_state
            .then(reconnection_time)
            // The remainder is meaningless as an ordering, but required so that we
            // have a total order on `MetaAddr` values: self and other must compare
            // as Ordering::Equal iff they are equal.
            .then(ip_numeric)
            .then(self.addr.port().cmp(&other.addr.port()))
            .then(self.services.bits().cmp(&other.services.bits()))
    }
}

impl PartialOrd for MetaAddr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl ZcashSerialize for MetaAddr {
    fn zcash_serialize<W: Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        writer.write_u32::<LittleEndian>(self.get_last_seen().timestamp() as u32)?;
        writer.write_u64::<LittleEndian>(self.services.bits())?;
        writer.write_socket_addr(self.addr)?;
        Ok(())
    }
}

impl ZcashDeserialize for MetaAddr {
    fn zcash_deserialize<R: Read>(mut reader: R) -> Result<Self, SerializationError> {
        let last_seen = Utc.timestamp(reader.read_u32::<LittleEndian>()? as i64, 0);
        let services = PeerServices::from_bits_truncate(reader.read_u64::<LittleEndian>()?);
        let addr = reader.read_socket_addr()?;

        Ok(MetaAddr::new_gossiped(&addr, &services, &last_seen))
    }
}

/// A serialized meta addr has a 4 byte time, 8 byte services, 16 byte IP addr, and 2 byte port
const META_ADDR_SIZE: usize = 4 + 8 + 16 + 2;

impl TrustedPreallocate for MetaAddr {
    fn max_allocation() -> u64 {
        // Since a maximal serialized Vec<MetAddr> uses at least three bytes for its length (2MB  messages / 30B MetaAddr implies the maximal length is much greater than 253)
        // the max allocation can never exceed (MAX_PROTOCOL_MESSAGE_LEN - 3) / META_ADDR_SIZE
        ((MAX_PROTOCOL_MESSAGE_LEN - 3) / META_ADDR_SIZE) as u64
    }
}
