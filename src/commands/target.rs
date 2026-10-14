//! What a read verb reads: a memory as it stands, or the memory this host is bound to as it will be
//! after this host's next push; and the `--memory` a drill-down repeats to read the same.
//!
//! The hooks `funes add` installs publish at session boundaries, so between two of them a turn
//! another session on this host just indexed is in the local memory and not yet in the bound one.
//! A read naming no memory reads the bound memory with those turns, so they are in reach without
//! a push. A read naming a memory, the bound one included, reads it as it stands.

use super::push;
use super::recall::{memory_hint, Candidates, Search, SessionPool};
use crate::memory::{self, Memory, MemoryState};
use anyhow::Result;

/// What a read verb reads, and the `--memory` a drill-down repeats to read the same.
#[derive(Clone, Debug)]
pub struct Target {
    memory: Memory,
    /// The memory the call named, as it named it. `None` for a call naming none, whose `→ get`
    /// hints name none either, so a drill-down reads as the search did.
    named: Option<String>,
    /// Read as this host's bound memory: its own rows, and this host's turns not yet pushed to
    /// it, which come from the local memory.
    bound: bool,
}

/// A memory handed to a verb is read as it stands, named by its label.
impl From<Memory> for Target {
    fn from(memory: Memory) -> Self {
        Target {
            named: Some(memory.label()),
            memory,
            bound: false,
        }
    }
}

impl Target {
    /// What a call reads: the memory it names, as it stands. Naming none, the memory this host is
    /// bound to, as it will be after the next push, and the local memory when the host is local.
    pub fn resolve(spec: Option<&str>) -> Result<Target> {
        Ok(resolve_with(bound()?, spec))
    }

    /// What a call naming no memory reads on a server started on `spec`: `spec` as the bound
    /// memory when this host is bound to it, as it stands otherwise. A server started on none
    /// serves what [`Target::resolve`] gives a call naming none.
    pub fn served(spec: Option<&str>) -> Result<Target> {
        Ok(served_with(bound()?, spec))
    }

    /// `memory` read as this host's bound memory, as a call naming none reads it on a host bound
    /// to it.
    pub fn bound(memory: Memory) -> Target {
        Target {
            memory,
            named: None,
            bound: true,
        }
    }

    /// The memory read, before this host's unpushed turns.
    pub fn memory(&self) -> &Memory {
        &self.memory
    }

    /// The memory read, taken out of the target.
    pub fn into_memory(self) -> Memory {
        self.memory
    }

    /// Whether the memory is read as this host's bound memory.
    pub fn is_bound(&self) -> bool {
        self.bound
    }

    /// The `--memory` a `→ get` hint carries: the memory as the call named it, nothing for a call
    /// naming none.
    pub fn hint(&self) -> String {
        memory_hint(self.named.as_deref())
    }

    /// The remote this host's unpushed turns are read on top of: the bound target's.
    fn overlaid(&self) -> Option<&str> {
        match &self.memory {
            Memory::Remote { uri } if self.bound => Some(uri),
            _ => None,
        }
    }

    /// The pools a recall ranks: the memory's own candidates, and for the bound memory the local
    /// memory's, kept to this host's turns not yet pushed. The local search runs while the remote
    /// one waits on the network. Every hit carries the `--memory` its drill-down repeats.
    pub(crate) async fn candidates(
        &self,
        search: &Search,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<Vec<Candidates>> {
        let (own, owed) = tokio::join!(search.candidates(&self.memory, progress), async {
            match self.overlaid() {
                Some(uri) => unpushed(search, uri).await,
                None => Ok(None),
            }
        });
        let mut pools = vec![own?];
        pools.extend(owed?);
        for pool in &mut pools {
            pool.hint(self.named.as_deref().unwrap_or_default());
        }
        Ok(pools)
    }

    /// The pools a listing merges: the memory's sessions, and for the bound memory those this host
    /// still owes rows, which come from the local memory whole instead. The local side is read
    /// while the remote one waits on the network.
    pub(crate) async fn session_pools(&self) -> Result<Vec<SessionPool>> {
        let Some(uri) = self.overlaid() else {
            return Ok(vec![SessionPool::open(&self.memory).await?]);
        };
        let (pool, owed) = tokio::join!(SessionPool::open(&self.memory), async {
            let owed = push::owed_sessions(uri).await?;
            if owed.is_empty() {
                return Ok(None);
            }
            let mut local = SessionPool::open(&Memory::local()).await?;
            local.retain(|s| owed.contains(&s.session_id));
            anyhow::Ok(Some((local, owed)))
        });
        let mut pool = pool?;
        let Some((local, owed)) = owed? else {
            return Ok(vec![pool]);
        };
        pool.retain(|s| !owed.contains(&s.session_id));
        Ok(vec![pool, local])
    }

    /// Where `session_id` is read from: the local memory when the bound memory is still owed rows
    /// of it, since the local memory holds the session whole; else the memory itself.
    pub(crate) async fn holder(self, session_id: &str) -> Result<Memory> {
        if let Some(uri) = self.overlaid() {
            if push::owes_session(uri, session_id).await? {
                return Ok(Memory::local());
            }
        }
        Ok(self.memory)
    }
}

/// This host's turns not yet pushed to the remote at `uri`: the local memory's candidates for
/// `search`, kept to the rows missing from this host's receipt for it. `None` unless this host has
/// pushed there. The hits hint no memory, as a call naming none does; [`Target::candidates`] sets
/// the hint of every pool it ranks.
pub async fn unpushed(search: &Search, uri: &str) -> Result<Option<Candidates>> {
    let Some(pushed) = push::load_pushed(uri) else {
        return Ok(None);
    };
    // A local memory with no rows has nothing unpushed, and searching it is an error.
    if !matches!(Memory::local().state().await?, MemoryState::Ready(_)) {
        return Ok(None);
    }
    let mut pool = search.candidates(&Memory::local(), &|_| ()).await?;
    pool.retain(|h| !pushed.contains(&h.id));
    pool.hint("");
    Ok(Some(pool))
}

/// The memory this host is bound to, when it is not local.
fn bound() -> Result<Option<Memory>> {
    Ok(memory::binding()?.map(|spec| Memory::parse(&spec)))
}

/// What a call reads on a host bound to `bound`.
fn resolve_with(bound: Option<Memory>, spec: Option<&str>) -> Target {
    if let Some(spec) = named(spec) {
        return Target {
            memory: Memory::parse(spec),
            named: Some(spec.to_string()),
            bound: false,
        };
    }
    match bound {
        Some(memory) => as_bound(memory),
        None => as_default(Memory::local()),
    }
}

/// What a server started on `spec` serves a call naming no memory, on a host bound to `bound`.
fn served_with(bound: Option<Memory>, spec: Option<&str>) -> Target {
    let Some(spec) = named(spec) else {
        return resolve_with(bound, None);
    };
    let memory = Memory::parse(spec);
    if bound.is_some_and(|bound| bound.label() == memory.label()) {
        as_bound(memory)
    } else {
        as_default(memory)
    }
}

/// A spec that names a memory: one that is not blank.
fn named(spec: Option<&str>) -> Option<&str> {
    spec.map(str::trim).filter(|s| !s.is_empty())
}

/// What a call naming none reads on a host bound to `memory`: a remote as it will be after the
/// next push; a memory at a local path as it stands, since nothing is pushed to it.
fn as_bound(memory: Memory) -> Target {
    let bound = matches!(memory, Memory::Remote { .. });
    Target {
        memory,
        named: None,
        bound,
    }
}

/// `memory` as it stands, for a call naming none.
fn as_default(memory: Memory) -> Target {
    Target {
        memory,
        named: None,
        bound: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound_to(spec: &str) -> Option<Memory> {
        Some(Memory::parse(spec))
    }

    #[test]
    fn a_call_naming_a_memory_reads_it_as_it_stands_and_its_hint_repeats_the_name() {
        let t = resolve_with(bound_to("acme/kb"), Some("acme/kb"));
        assert_eq!(t.memory().label(), "hf://datasets/acme/kb");
        assert!(!t.bound);
        assert_eq!(t.hint(), " --memory acme/kb");
        let t = resolve_with(bound_to("acme/kb"), Some("local"));
        assert!(t.memory().is_default_local() && !t.bound);
        assert_eq!(t.hint(), " --memory local");
    }

    #[test]
    fn a_call_naming_none_reads_the_bound_memory_as_after_the_next_push_and_hints_none() {
        let t = resolve_with(bound_to("acme/kb"), None);
        assert_eq!(t.memory().label(), "hf://datasets/acme/kb");
        assert!(t.bound);
        assert_eq!(t.hint(), "");
        assert!(resolve_with(bound_to("acme/kb"), Some(" ")).bound);
    }

    #[test]
    fn a_call_naming_none_reads_the_local_memory_on_a_local_host() {
        let t = resolve_with(None, None);
        assert!(t.memory().is_default_local() && !t.bound);
        assert_eq!(t.hint(), "");
    }

    #[test]
    fn a_host_bound_to_a_local_path_reads_it_as_it_stands() {
        let t = resolve_with(bound_to("/srv/kb"), None);
        assert_eq!(t.memory().label(), "/srv/kb");
        assert!(!t.bound);
    }

    /// A memory handed to a verb is a named read, so its hints name it.
    #[test]
    fn a_memory_handed_to_a_verb_is_named_by_its_label() {
        let t = Target::from(Memory::parse("acme/kb"));
        assert!(!t.bound);
        assert_eq!(t.hint(), " --memory hf://datasets/acme/kb");
    }

    /// `funes add` bakes the binding into the server's registration, so on that server a call
    /// naming none is one on the bound memory. A server started by hand on another memory reads
    /// it as it stands, and the call still named none, so its hints name none.
    #[test]
    fn a_server_serves_its_memory_as_bound_only_when_this_host_is_bound_to_it() {
        assert!(served_with(bound_to("acme/kb"), Some("acme/kb")).bound);
        // The binding is kept as typed, and a server may be started on another spelling of it.
        assert!(served_with(bound_to("acme/kb"), Some("hf://datasets/acme/kb")).bound);
        let other = served_with(bound_to("acme/kb"), Some("acme/other"));
        assert!(!other.bound);
        assert_eq!(other.hint(), "");
        assert!(served_with(bound_to("acme/kb"), None).bound);
        assert!(!served_with(None, Some("acme/kb")).bound);
        let local = served_with(bound_to("acme/kb"), Some("local"));
        assert!(local.memory().is_default_local() && !local.bound);
    }
}
