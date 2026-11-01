//! Who owns a recorded model call: this session, an earlier session it copied it
//! from, or nobody provable.
//!
//! A fork copies its parent's entries (unchanged native entry IDs) and a continuation
//! segment may repeat earlier segments. Counting those copies again would charge the
//! same request twice, and a copy's reset money cannot tell them apart. Ownership is
//! therefore decided from *already discovered, explicitly linked* sources only:
//! the fork parent named by the header, and the earlier members of a continuation
//! chain. Their usage-bearing entry IDs (and confirmed provider response IDs) are
//! kept as bounded evidence; arbitrary `parentSession` paths are never opened, and a
//! timestamp is never evidence.
//!
//! * Owned: no linked predecessor holds the call, or the identity differs (the same
//!   entry ID with a different confirmed response ID is a new request).
//! * Inherited: a linked predecessor holds the call (same entry ID with a compatible
//!   response ID, or the same confirmed response ID under another entry ID).
//! * Unknown: the link is ambiguous or unresolved, or the predecessor's evidence is
//!   missing or incomplete, so a copy cannot be ruled out.

use std::collections::{HashMap, HashSet};

use crate::domain::{SessionKey, SessionLink, SessionMetadata};
use crate::providers::omp::OmpCatalog;
use crate::providers::omp::links::REJECTED_CONTINUATION;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OmpOwnership {
    Owned,
    Inherited,
    Unknown,
}

/// The relationship facts of one discovered source that ownership consults.
#[derive(Clone, Debug, Default)]
pub(super) struct SourceLinks {
    fork: Option<SessionLink>,
    parent: Option<SessionLink>,
    sidechain: bool,
    /// Chain members in order (the logical root first); empty for a plain session.
    members: Vec<SessionKey>,
}

impl SourceLinks {
    pub(super) fn of(meta: &SessionMetadata) -> Self {
        Self {
            fork: meta.lineage.fork.clone(),
            parent: meta.lineage.parent.clone(),
            sidechain: meta.lineage.sidechain,
            members: meta.members.clone(),
        }
    }
}

/// Usage-bearing entries of one source: entry ID → (confirmed response ID, counter
/// fingerprint), plus the response IDs.
#[derive(Default)]
pub(super) struct Evidence {
    ids: HashMap<String, (Option<String>, u64)>,
    responses: HashSet<String>,
    complete: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Match,
    NoMatch,
    Unproven,
}

impl OmpCatalog {
    /// Replaces every source's relationship facts (the catalog does this after each
    /// publish) and rebuilds the continuation index.
    pub(super) fn set_links(&mut self, links: HashMap<SessionKey, SourceLinks>) {
        self.links = links;
        self.reindex_members();
    }

    /// Records the relationship facts of `meta`, e.g. for a session whose catalog
    /// pass has not reported it.
    pub fn seed_links(&mut self, meta: &SessionMetadata) {
        self.links.insert(meta.key.clone(), SourceLinks::of(meta));
        self.reindex_members();
    }

    fn reindex_members(&mut self) {
        self.member_of.clear();
        for (root, links) in &self.links {
            for (index, member) in links.members.iter().enumerate() {
                self.member_of.insert(member.clone(), (root.clone(), index));
            }
        }
    }

    /// The sources whose entries prove what `source` copied: the members of the
    /// resolved fork parent (or the parent itself). `None` when `source` names no
    /// resolved fork parent.
    pub fn fork_proof_sources(&self, source: &SessionKey) -> Option<Vec<SessionKey>> {
        let Some(SessionLink::Resolved(parent)) = &self.links.get(source)?.fork else {
            return None;
        };
        Some(self.chain_of(parent))
    }

    fn chain_of(&self, root: &SessionKey) -> Vec<SessionKey> {
        match self.links.get(root) {
            Some(links) if !links.members.is_empty() => links.members.clone(),
            _ => vec![root.clone()],
        }
    }

    /// Starts (or restarts) the evidence of `source`, empty and incomplete.
    pub fn begin_evidence(&mut self, source: &SessionKey) {
        if let Some(old) = self.evidence.remove(source) {
            self.evidence_len -= old.ids.len();
        }
        self.evidence.insert(source.clone(), Evidence::default());
    }

    /// Adds a usage-bearing entry of `source`; `false` when the evidence ceiling is
    /// reached (the entry is then not recorded).
    pub fn record_evidence(
        &mut self,
        source: &SessionKey,
        entry_id: &str,
        response_id: Option<&str>,
        fingerprint: u64,
    ) -> bool {
        let Some(evidence) = self.evidence.get_mut(source) else {
            return false;
        };
        if evidence.ids.contains_key(entry_id) {
            // Another confirmed response under a known entry ID is still evidence.
            if let Some(response) = response_id
                && !evidence.responses.contains(response)
            {
                if self.evidence_len >= self.evidence_cap {
                    return false;
                }
                self.evidence_len += 1;
                evidence.responses.insert(response.to_owned());
            }
            return true;
        }
        if self.evidence_len >= self.evidence_cap {
            return false;
        }
        self.evidence_len += 1;
        evidence.ids.insert(
            entry_id.to_owned(),
            (response_id.map(str::to_owned), fingerprint),
        );
        if let Some(response) = response_id {
            evidence.responses.insert(response.to_owned());
        }
        true
    }

    /// Marks the evidence of `source` as covering every usage-bearing entry.
    pub fn complete_evidence(&mut self, source: &SessionKey) {
        if let Some(evidence) = self.evidence.get_mut(source) {
            evidence.complete = true;
        }
    }

    /// Forgets all evidence (cancelled work).
    pub fn clear_evidence(&mut self) {
        self.evidence.clear();
        self.evidence_len = 0;
    }

    /// What `source` recorded for `entry_id`: its confirmed response ID and counter
    /// fingerprint.
    pub fn evidence_entry(
        &self,
        source: &SessionKey,
        entry_id: &str,
    ) -> Option<&(Option<String>, u64)> {
        self.evidence.get(source)?.ids.get(entry_id)
    }

    fn check(&self, predecessor: &SessionKey, entry_id: &str, response: Option<&str>) -> Verdict {
        let Some(evidence) = self.evidence.get(predecessor) else {
            return Verdict::Unproven;
        };
        let by_response = response.is_some_and(|r| evidence.responses.contains(r));
        let matched = by_response
            || match evidence.ids.get(entry_id) {
                Some((recorded, _)) => match (recorded.as_deref(), response) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                },
                None => false,
            };
        if matched {
            Verdict::Match
        } else if evidence.complete {
            Verdict::NoMatch
        } else {
            Verdict::Unproven
        }
    }

    fn ownership(
        &self,
        entry_id: &str,
        response: Option<&str>,
        source: &SessionKey,
    ) -> OmpOwnership {
        let links = self.links.get(source);
        let mut matched = false;
        let mut unproven = false;
        let mut note = |verdict: Verdict| match verdict {
            Verdict::Match => matched = true,
            Verdict::Unproven => unproven = true,
            Verdict::NoMatch => {}
        };
        match links.and_then(|l| l.fork.as_ref()) {
            None => {}
            Some(SessionLink::Resolved(parent)) => match self.links.get(parent) {
                Some(parent_links) if !parent_links.members.is_empty() => {
                    for member in &parent_links.members {
                        note(self.check(member, entry_id, response));
                    }
                }
                _ => note(self.check(parent, entry_id, response)),
            },
            // The parent cannot be named, so a copy cannot be ruled out.
            Some(_) => return OmpOwnership::Unknown,
        }
        // A continuation whose predecessor was rejected may have copied from it.
        if let Some(links) = links
            && !links.sidechain
            && match &links.parent {
                Some(SessionLink::Ambiguous) => true,
                Some(SessionLink::Unresolved(text)) => text.starts_with(REJECTED_CONTINUATION),
                _ => false,
            }
        {
            return OmpOwnership::Unknown;
        }
        if let Some((root, index)) = self.member_of.get(source)
            && let Some(chain) = self.links.get(root)
        {
            for earlier in chain.members.iter().take(*index) {
                note(self.check(earlier, entry_id, response));
            }
        }
        if matched {
            OmpOwnership::Inherited
        } else if unproven {
            OmpOwnership::Unknown
        } else {
            OmpOwnership::Owned
        }
    }
}

/// Whether the call recorded at `entry_id` (with the provider's confirmed
/// `response_id`, if any) in `source` is new spend of `source`, copied from a linked
/// source, or cannot be told. `lineage` supplies the explicit links and the
/// predecessors' evidence it already holds; nothing is opened here.
pub fn owned_request(
    entry_id: &str,
    response_id: Option<&str>,
    source: &SessionKey,
    lineage: &OmpCatalog,
) -> OmpOwnership {
    lineage.ownership(entry_id, response_id, source)
}
