//! What a read verb reads: a memory as it stands, or the memory this host is bound to as it will be
//! after this host's next push.
//!
//! The hooks `funes add` installs publish at session boundaries, so between two of them a turn
//! another session on this host just indexed is in the local memory and not yet in the bound one.
//! A read naming no memory reads the bound memory with those turns, so they are in reach without
//! a push. A read naming a memory, the bound one included, reads it as it stands.

use super::push;
use super::recall::{Candidates, Search, SessionPool};
use crate::memory::{self, Memory, MemoryState};
use anyhow::Result;

/// What a read verb reads.
#[derive(Clone, Debug)]
pub enum Target {
    /// `memory`, as it stands.
    Exact(Memory),
    /// The remote this host is bound to, as it will be after this host's next push: its own rows,
    /// and this host's turns not yet pushed to it, which come from the local memory.
    Bound(Memory),
}

/// A memory on its own reads as it stands.
impl From<Memory> for Target {
    fn from(memory: Memory) -> Self {
        Target::Exact(memory)
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

    /// The memory read, before this host's unpushed turns.
    pub fn memory(&self) -> &Memory {
        match self {
            Target::Exact(memory) | Target::Bound(memory) => memory,
        }
    }

    /// The remote this host's unpushed turns are read on top of: the bound target's.
    fn overlaid(&self) -> Option<&str> {
        match self {
            Target::Bound(Memory::Remote { uri }) => Some(uri),
            _ => None,
        }
    }

    /// The pools a recall ranks: the memory's own candidates, and for the bound memory the local
    /// memory's, kept to this host's turns not yet pushed. The local search runs while the remote
    /// one waits on the network.
    pub(crate) async fn candidates(
        &self,
        search: &Search,
        progress: &(dyn Fn(&str) + Sync),
    ) -> Result<Vec<Candidates>> {
        let (own, owed) = tokio::join!(search.candidates(self.memory(), progress), async {
            match self.overlaid() {
                Some(uri) => unpushed(search, uri).await,
                None => Ok(None),
            }
        });
        let mut pools = vec![own?];
        pools.extend(owed?);
        Ok(pools)
    }

    /// The pools a listing merges: the memory's sessions, and for the bound memory those this host
    /// still owes rows, which come from the local memory whole instead. The local side is read
    /// while the remote one waits on the network.
    pub(crate) async fn session_pools(&self) -> Result<Vec<SessionPool>> {
        let Some(uri) = self.overlaid() else {
            return Ok(vec![SessionPool::open(self.memory()).await?]);
        };
        let (pool, owed) = tokio::join!(SessionPool::open(self.memory()), async {
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
        Ok(match self {
            Target::Exact(memory) | Target::Bound(memory) => memory,
        })
    }
}

/// This host's turns not yet pushed to the remote at `uri`: the local memory's candidates for
/// `search`, kept to the rows missing from this host's receipt for it. `None` unless this host has
/// pushed there.
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
    Ok(Some(pool))
}

/// The memory this host is bound to, when it is not local.
fn bound() -> Result<Option<Memory>> {
    Ok(memory::binding()?.map(|spec| Memory::parse(&spec)))
}

/// What a call reads on a host bound to `bound`.
fn resolve_with(bound: Option<Memory>, spec: Option<&str>) -> Target {
    if let Some(spec) = named(spec) {
        return Target::Exact(Memory::parse(spec));
    }
    match bound {
        Some(memory) => as_bound(memory),
        None => Target::Exact(Memory::local()),
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
        Target::Exact(memory)
    }
}

/// A spec that names a memory: one that is not blank.
fn named(spec: Option<&str>) -> Option<&str> {
    spec.map(str::trim).filter(|s| !s.is_empty())
}

/// A bound remote reads as it will be after the next push. A memory at a local path is never
/// pushed to, so it reads as it stands.
fn as_bound(memory: Memory) -> Target {
    match memory {
        Memory::Remote { .. } => Target::Bound(memory),
        Memory::Local { .. } => Target::Exact(memory),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound_to(spec: &str) -> Option<Memory> {
        Some(Memory::parse(spec))
    }

    #[test]
    fn a_call_naming_a_memory_reads_it_as_it_stands() {
        assert!(matches!(
            resolve_with(bound_to("acme/kb"), Some("acme/kb")),
            Target::Exact(m) if m.label() == "hf://datasets/acme/kb"
        ));
        assert!(matches!(
            resolve_with(bound_to("acme/kb"), Some("local")),
            Target::Exact(m) if m.is_default_local()
        ));
    }

    #[test]
    fn a_call_naming_none_reads_the_bound_memory_as_after_the_next_push() {
        assert!(matches!(
            resolve_with(bound_to("acme/kb"), None),
            Target::Bound(m) if m.label() == "hf://datasets/acme/kb"
        ));
        assert!(matches!(resolve_with(bound_to("acme/kb"), Some(" ")), Target::Bound(_)));
    }

    #[test]
    fn a_call_naming_none_reads_the_local_memory_on_a_local_host() {
        assert!(matches!(resolve_with(None, None), Target::Exact(m) if m.is_default_local()));
    }

    #[test]
    fn a_host_bound_to_a_local_path_reads_it_as_it_stands() {
        assert!(matches!(
            resolve_with(bound_to("/srv/kb"), None),
            Target::Exact(m) if m.label() == "/srv/kb"
        ));
    }

    /// `funes add` bakes the binding into the server's registration, so on that server a call
    /// naming none is one on the bound memory. A server started by hand on another memory reads
    /// it as it stands.
    #[test]
    fn a_server_serves_its_memory_as_bound_only_when_this_host_is_bound_to_it() {
        assert!(matches!(
            served_with(bound_to("acme/kb"), Some("acme/kb")),
            Target::Bound(_)
        ));
        // The binding is kept as typed, and a server may be started on another spelling of it.
        assert!(matches!(
            served_with(bound_to("acme/kb"), Some("hf://datasets/acme/kb")),
            Target::Bound(_)
        ));
        assert!(matches!(
            served_with(bound_to("acme/kb"), Some("acme/other")),
            Target::Exact(_)
        ));
        assert!(matches!(served_with(bound_to("acme/kb"), None), Target::Bound(_)));
        assert!(matches!(served_with(None, Some("acme/kb")), Target::Exact(_)));
        assert!(matches!(
            served_with(bound_to("acme/kb"), Some("local")),
            Target::Exact(m) if m.is_default_local()
        ));
    }
}
