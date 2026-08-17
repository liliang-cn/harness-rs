//! Making the gate unforgeable.
//!
//! [`crate::Acceptance`] asks whether the work is really done. It is worth
//! nothing if the agent can edit the thing doing the asking — and that is not a
//! hypothetical: the reliably-observed failure is a model that cannot make a
//! test pass loosening the test until it does, then reporting success. The run
//! ends green, the gate was consulted, and it agreed, because by then it was a
//! different gate.
//!
//! So a check may declare the files that *define* it —
//! [`Acceptance::seals`](crate::Acceptance::seals). Those are digested before
//! the model gets its first turn and re-digested before any pass is accepted. A
//! difference means the contract moved during the run, and the run fails on
//! that ground alone, whatever the checks said.
//!
//! **What this enforces, precisely.** It detects that a sealed file's bytes
//! changed between the start of the run and the verdict. That is the whole
//! claim. It does not stop the write — the filesystem sandbox is what does
//! that, and sealing is the check for hosts that do not have one, or for paths
//! outside it. It also cannot see a file the check reads but did not declare;
//! `seals()` is a promise the check makes about itself, and a check that lies
//! about its inputs is not sealed no matter what this module does.
//!
//! Sealing is opt-in and empty by default, because some runs are *supposed* to
//! rewrite the tests. Seal what must not move.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A sealed file's state at a point in time.
///
/// `None` records "absent", which has to be distinguishable from any digest:
/// creating a contract file that was not there, or deleting one that was, are
/// both tampering and both would otherwise be invisible.
pub type Digest64 = Option<String>;

/// The digests of every sealed path, taken together.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealSet {
    /// Path → digest, ordered so the set serialises identically every time.
    pub entries: BTreeMap<PathBuf, Digest64>,
}

/// One file that moved while the run was in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealBreach {
    pub path: PathBuf,
    pub before: Digest64,
    pub after: Digest64,
}

impl SealBreach {
    /// Said the way a human reading a failed run needs to hear it.
    pub fn describe(&self) -> String {
        let what = match (&self.before, &self.after) {
            (Some(_), Some(_)) => "was modified",
            (None, Some(_)) => "was created",
            (Some(_), None) => "was deleted",
            (None, None) => "changed", // unreachable: equal states are not breaches
        };
        format!("{} {}", self.path.display(), what)
    }
}

impl SealSet {
    /// Digest every path, relative to `root` when the path is relative.
    ///
    /// An unreadable path is recorded as absent rather than as an error: the
    /// question is only whether it is the same at the end as at the start, and
    /// "could not read it either time" is a consistent answer.
    pub fn capture<I, P>(root: &Path, paths: I) -> Self
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let mut entries = BTreeMap::new();
        for p in paths {
            let rel = p.as_ref().to_path_buf();
            let full = if rel.is_absolute() {
                rel.clone()
            } else {
                root.join(&rel)
            };
            entries.insert(rel, digest_file(&full));
        }
        Self { entries }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every path whose digest differs from `self`.
    ///
    /// Compares over the union of both key sets, so a check whose declared
    /// seals somehow differ between capture and verify still reports rather
    /// than silently skipping the paths only one side knows about.
    pub fn breaches(&self, now: &SealSet) -> Vec<SealBreach> {
        let mut out = Vec::new();
        let mut keys: Vec<&PathBuf> = self.entries.keys().chain(now.entries.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            let before = self.entries.get(k).cloned().flatten();
            let after = now.entries.get(k).cloned().flatten();
            if before != after {
                out.push(SealBreach {
                    path: k.clone(),
                    before,
                    after,
                });
            }
        }
        out
    }
}

/// `sha256` of a file's bytes, hex-encoded; `None` when it cannot be read.
fn digest_file(path: &Path) -> Digest64 {
    let bytes = std::fs::read(path).ok()?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Some(format!("{:x}", h.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "harness-seal-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn an_untouched_contract_is_not_a_breach() {
        let d = tmp();
        std::fs::write(d.join("check.sh"), "exit 0").unwrap();
        let a = SealSet::capture(&d, ["check.sh"]);
        let b = SealSet::capture(&d, ["check.sh"]);
        assert!(a.breaches(&b).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn loosening_the_test_is_caught() {
        // The motivating case, written out: the check said the answer must be
        // 42, the model could not manage it, so the model edited the check.
        let d = tmp();
        let f = d.join("expected.txt");
        std::fs::write(&f, "42").unwrap();
        let before = SealSet::capture(&d, ["expected.txt"]);
        std::fs::write(&f, "any").unwrap();
        let after = SealSet::capture(&d, ["expected.txt"]);

        let b = before.breaches(&after);
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].path, PathBuf::from("expected.txt"));
        assert!(b[0].describe().contains("was modified"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn deleting_the_contract_is_a_breach_not_a_pass() {
        // Absent must not read as "nothing to compare, therefore fine" — that
        // would make `rm` the cheapest way through the gate.
        let d = tmp();
        let f = d.join("gone.txt");
        std::fs::write(&f, "contract").unwrap();
        let before = SealSet::capture(&d, ["gone.txt"]);
        std::fs::remove_file(&f).unwrap();
        let after = SealSet::capture(&d, ["gone.txt"]);

        let b = before.breaches(&after);
        assert_eq!(b.len(), 1);
        assert!(b[0].describe().contains("was deleted"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn creating_a_contract_that_was_absent_is_a_breach() {
        let d = tmp();
        let before = SealSet::capture(&d, ["appears.txt"]);
        std::fs::write(d.join("appears.txt"), "now here").unwrap();
        let after = SealSet::capture(&d, ["appears.txt"]);
        let b = before.breaches(&after);
        assert_eq!(b.len(), 1);
        assert!(b[0].describe().contains("was created"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_that_never_existed_is_not_a_breach() {
        let d = tmp();
        let a = SealSet::capture(&d, ["nope.txt"]);
        let b = SealSet::capture(&d, ["nope.txt"]);
        assert!(a.breaches(&b).is_empty(), "absent twice is consistent");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_digest_is_of_content_not_of_the_path() {
        // Two different files with identical bytes must digest the same, so a
        // check that renames its contract cannot pass by shuffling paths.
        let d = tmp();
        std::fs::write(d.join("a.txt"), "same").unwrap();
        std::fs::write(d.join("b.txt"), "same").unwrap();
        let s = SealSet::capture(&d, ["a.txt", "b.txt"]);
        let vals: Vec<_> = s.entries.values().cloned().collect();
        assert_eq!(vals[0], vals[1]);
        assert!(vals[0].is_some());
        let _ = std::fs::remove_dir_all(&d);
    }
}
