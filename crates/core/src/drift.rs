use sha2::{Digest, Sha256};

use crate::chezmoi::EvalFailure;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        let digest = Sha256::digest(bytes);
        Self(digest.into())
    }
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
    pub fn from_hex(s: &str) -> Option<Self> {
        if s.len() != 64 {
            return None;
        }
        let mut out = [0u8; 32];
        for (i, chunk) in s.as_bytes().chunks(2).enumerate() {
            let hi = (chunk[0] as char).to_digit(16)?;
            let lo = (chunk[1] as char).to_digit(16)?;
            out[i] = (hi * 16 + lo) as u8;
        }
        Some(Self(out))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitSignals {
    pub local_ahead: bool,
    pub remote_ahead: bool,
}

#[derive(Debug, Clone)]
pub struct StateProbe {
    /// Hash of the file at its target path; None = missing.
    pub destination: Option<ContentHash>,
    /// Hash of `chezmoi cat` output; Err = template/secret failure; Ok(None) = entry has no content (e.g. would be removed).
    pub rendered: Result<Option<ContentHash>, EvalFailure>,
    /// From `chezmoi state dump` entryState contentsSHA256; None = chezmoi never wrote this entry.
    pub last_written: Option<ContentHash>,
    pub git: GitSignals,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftClass {
    InSync,
    DestinationDrift,
    SourceAhead,
    RemoteAhead,
    LocalSourceDiverged,
    Conflict,
    EvalFailed,
}

pub fn classify(probe: &StateProbe) -> DriftClass {
    let rendered = match &probe.rendered {
        Err(_) => return DriftClass::EvalFailed,
        Ok(r) => *r,
    };
    let dest_drift = probe.destination != probe.last_written;
    let source_moved = rendered != probe.last_written;
    if probe.git.local_ahead && probe.git.remote_ahead {
        return if dest_drift {
            DriftClass::Conflict
        } else {
            DriftClass::LocalSourceDiverged
        };
    }
    let signals = u8::from(dest_drift) + u8::from(source_moved) + u8::from(probe.git.remote_ahead);
    match signals {
        0 => DriftClass::InSync,
        1 if dest_drift => DriftClass::DestinationDrift,
        1 if source_moved => DriftClass::SourceAhead,
        1 => DriftClass::RemoteAhead,
        _ => DriftClass::Conflict,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chezmoi::{EvalFailure, EvalFailureKind};

    fn h(n: u8) -> Option<ContentHash> {
        Some(ContentHash::of(&[n]))
    }
    fn probe(
        dest: Option<ContentHash>,
        rendered: Option<ContentHash>,
        last: Option<ContentHash>,
        local_ahead: bool,
        remote_ahead: bool,
    ) -> StateProbe {
        StateProbe {
            destination: dest,
            rendered: Ok(rendered),
            last_written: last,
            git: GitSignals {
                local_ahead,
                remote_ahead,
            },
        }
    }

    #[test]
    fn classification_table() {
        use DriftClass::*;
        let cases = [
            (probe(h(1), h(1), h(1), false, false), InSync),
            (probe(h(2), h(1), h(1), false, false), DestinationDrift),
            (probe(None, h(1), h(1), false, false), DestinationDrift), // deleted on disk
            (probe(h(1), h(2), h(1), false, false), SourceAhead),
            (probe(h(1), h(1), h(1), false, true), RemoteAhead),
            (probe(h(1), h(1), h(1), true, true), LocalSourceDiverged),
            (probe(h(2), h(1), h(1), true, true), Conflict), // diverged + dest drift
            (probe(h(2), h(3), h(1), false, false), Conflict), // dest + source moved
            (probe(h(2), h(1), h(1), false, true), Conflict), // dest + remote
            (probe(h(1), h(1), None, false, false), Conflict), // never applied but present: dest+source signals
        ];
        for (i, (p, expected)) in cases.iter().enumerate() {
            assert_eq!(classify(p), *expected, "case {i}");
        }
    }

    #[test]
    fn eval_failure_dominates() {
        let p = StateProbe {
            destination: h(2),
            rendered: Err(EvalFailure {
                kind: EvalFailureKind::TemplateError,
                raw_stderr: String::new(),
                hint: String::new(),
            }),
            last_written: h(1),
            git: GitSignals {
                local_ahead: true,
                remote_ahead: true,
            },
        };
        assert_eq!(classify(&p), DriftClass::EvalFailed);
    }

    #[test]
    fn hash_roundtrip() {
        let h = ContentHash::of(b"abc");
        assert_eq!(ContentHash::from_hex(&h.to_hex()), Some(h));
    }
}
