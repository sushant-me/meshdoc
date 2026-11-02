//! Identifier and epoch types.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident, $n:expr, $doc:expr) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name(pub [u8; $n]);

        impl $name {
            pub fn random() -> Self {
                use rand::RngCore;
                let mut b = [0u8; $n];
                rand::rngs::OsRng.fill_bytes(&mut b);
                Self(b)
            }
            pub fn from_slice(s: &[u8]) -> Option<Self> {
                if s.len() != $n { return None; }
                let mut b = [0u8; $n];
                b.copy_from_slice(s);
                Some(Self(b))
            }
            pub fn as_bytes(&self) -> &[u8; $n] { &self.0 }
            pub fn to_hex(&self) -> String { hex(&self.0) }
            pub fn from_hex(s: &str) -> Option<Self> {
                let raw = unhex(s)?;
                Self::from_slice(&raw)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.to_hex())
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}(", stringify!($name))?;
                write!(f, "{}", &self.to_hex()[..16])?;
                write!(f, ")")
            }
        }
    };
}

id_type!(WorkspaceId, 16, "Identifier of a workspace.\n\n**Never derived from a key.** Rotation of the workspace key must not change the\nworkspace id, otherwise every relay mailbox and stored reference breaks.");
id_type!(DocId, 16, "Identifier of a document within a workspace.");
id_type!(PeerId, 16, "Short fingerprint of a device identity.");
id_type!(ChangeId, 16, "Content address of an encrypted change envelope.");

/// Membership epoch. Each key rotation produces a new epoch.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default, Serialize, Deserialize)]
pub struct Epoch(pub u32);

impl Epoch {
    pub const GENESIS: Epoch = Epoch(0);
    pub fn next(self) -> Epoch { Epoch(self.0 + 1) }
    pub fn as_u32(self) -> u32 { self.0 }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { write!(f, "e{}", self.0) }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const T: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(T[(b >> 4) as usize] as char);
        s.push(T[(b & 0x0f) as usize] as char);
    }
    s
}

pub(crate) fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 { return None; }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(s.len() / 2);
    let val = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < b.len() {
        out.push((val(b[i])? << 4) | val(b[i + 1])?);
        i += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_ids_are_random_and_not_derived() {
        let a = WorkspaceId::random();
        let b = WorkspaceId::random();
        assert_ne!(a, b);
        assert_eq!(a.to_hex().len(), 32);
    }

    #[test]
    fn hex_roundtrip() {
        let id = DocId::random();
        assert_eq!(DocId::from_hex(&id.to_hex()).unwrap(), id);
    }

    #[test]
    fn epoch_advances() {
        assert_eq!(Epoch::GENESIS.next(), Epoch(1));
        assert_eq!(Epoch(1).next(), Epoch(2));
    }
}
