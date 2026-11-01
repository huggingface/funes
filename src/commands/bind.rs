//! `funes bind <memory>`: the memory this host reads and publishes to, every installed agent
//! following. Also what `add` and `update` do with the binding: resolve a memory the user named,
//! bind the host, bring a host from before the binding forward, and the first push to a memory on
//! the Hub.

use super::add::{prepare_agent, setup_add};
use super::index;
use super::push;
use crate::agents::registry;
use crate::hub;
use crate::inference::EmbeddingModel;
use crate::memory::dataset;
use crate::memory::{self, receipt, Memory, MemoryState, Reachability};
use crate::traces::spool;
use crate::ui::prompt::confirm;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::Path;

/// A resolved memory binding: the memory spec, and whether funes just created the repo this run
/// (its first push then skips the wrong-memory guard: an empty repo the user just asked for is not
/// the wrong memory).
#[derive(Clone)]
pub(crate) struct Resolved {
    pub(crate) memory: String,
    pub(crate) created: bool,
}

/// The memory `spec` names, checked on the Hub (a typo guard, offering to create it); `None` for
/// `local`.
pub(crate) async fn resolve_named(spec: &str) -> Result<Option<Resolved>> {
    match spec.trim() {
        "" | "local" => Ok(None),
        spec => Ok(Some(Resolved {
            memory: spec.to_string(),
            created: ensure_remote_exists(spec).await?,
        })),
    }
}

/// `funes bind`: bind this host to `spec`, `local` to unbind, every installed agent following, then
/// the first push when this host has a local memory and owes one.
pub async fn run(spec: &str) -> Result<()> {
    let resolved = resolve_named(spec).await?;
    bind_host(resolved.clone(), None).await?;
    let Some(Resolved { memory, created }) = resolved else {
        return Ok(());
    };
    if owes_first_push(&memory, created).await? && index::local_memory_model().await?.is_some() {
        first_push(&memory, created).await.with_context(|| {
            format!(
                "this host is bound to {memory}, but the first push there did not go through — run \
                 `funes push {memory}` at a terminal to publish there"
            )
        })?;
    }
    Ok(())
}

/// Bind this host to `resolved`, `None` for its local memory: check its embedding model against
/// the local memory's (a host with none takes it), record it, and run every installed agent's
/// setup on it, `except` the one an add is about to install. The record stands whatever the setups
/// do: an agent that fails to follow is reported, and `bind` again brings it along. The first push
/// is the caller's.
pub(crate) async fn bind_host(resolved: Option<Resolved>, except: Option<&str>) -> Result<()> {
    let memory = resolved.as_ref().map(|r| r.memory.as_str());
    if let Some(memory) = memory {
        let here = index::local_memory_model().await?;
        match (bound_model(memory, here.is_none()).await?, here) {
            (Some(there), Some(here)) if there != here => bail!(
                "{memory} is embedded with {}, and the local memory with {}: a memory takes one embedding model",
                there.id(),
                here.id()
            ),
            (Some(there), None) => {
                eprintln!(
                    "funes: embedding the local memory with {}, the model of {memory}.",
                    there.id()
                );
                index::ensure_local_memory(there).await?;
            }
            _ => {}
        }
    }
    memory::bind(memory)?;
    let root = registry::default_root()?;
    // The record replaces what an older funes noted beside each agent, whatever its setup does.
    for id in registry::registered_ids(&root) {
        registry::bind(&root, &id, None)?;
    }
    let mut followed = Vec::new();
    let mut failed = Vec::new();
    for id in registry::registered_ids(&root)
        .into_iter()
        .filter(|id| spool::is_id(id) && except != Some(id.as_str()))
    {
        match follow(&root, &id, memory).await {
            Ok(()) => followed.push(id),
            Err(e) => {
                eprintln!("the {id} integration could not follow: {e:#}");
                failed.push(id);
            }
        }
    }
    let bound = match memory {
        Some(memory) => format!("bound this host to {memory}"),
        None => "this host is local".to_string(),
    };
    if followed.is_empty() {
        println!("{bound}.");
    } else {
        println!("{bound}: {} follow.", followed.join(", "));
    }
    if !failed.is_empty() {
        bail!(
            "{} did not follow — fix what it says above, then run `funes bind` again",
            failed.join(", ")
        );
    }
    Ok(())
}

/// Run `id`'s installed setup on `memory`.
async fn follow(root: &Path, id: &str, memory: Option<&str>) -> Result<()> {
    let integration = prepare_agent(id, registry::speaks_another_contract(root, id), None)
        .await?
        .install(root)?;
    setup_add(&integration, memory)
}

/// Bind a host from before the binding as an older funes bound its agents: the one memory they
/// noted becomes the host's, every installed agent following, the ones that older funes left
/// local included, `except` the one an add is about to install. Agents that noted different
/// memories come back as the sentence for the caller to print or refuse on, and nothing is bound.
pub(crate) async fn migrate_bindings(root: &Path, except: Option<&str>) -> Result<Option<String>> {
    if memory::binding()?.is_some() {
        return Ok(None);
    }
    let groups = legacy_bindings(root)?;
    match groups.as_slice() {
        [] => Ok(None),
        [(spec, ids)] => {
            println!(
                "an older funes bound {} to {spec}; binding this host to it.",
                ids.join(", ")
            );
            let memory = Resolved {
                memory: spec.clone(),
                created: false,
            };
            bind_host(Some(memory), except).await?;
            Ok(None)
        }
        several => Ok(Some(disagreement(several))),
    }
}

/// The line `funes status` adds on a host from before the binding: what an older funes bound its
/// agents to, or that they disagree. `None` on a host `bound` already, or whose agents noted
/// nothing.
pub fn legacy_note(bound: Option<&str>) -> Result<Option<String>> {
    if bound.is_some() {
        return Ok(None);
    }
    Ok(match legacy_bindings(&registry::default_root()?)?.as_slice() {
        [] => None,
        [(spec, ids)] => Some(format!(
            "{} bound to {spec} by an older funes; `funes update` binds this host to it",
            are(ids)
        )),
        several => Some(disagreement(several)),
    })
}

/// The memories an older funes bound this host's agents to, each as first typed with the agents
/// bound to it. A binding is kept as typed, so one memory may be noted under two spellings. A
/// note saying `local` is no binding: funes never wrote one, and taken as one it would be handed
/// to the setups as a memory.
fn legacy_bindings(root: &Path) -> Result<Vec<(String, Vec<String>)>> {
    let mut by_label: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for (spec, ids) in registry::bindings(root)? {
        if spec == "local" {
            continue;
        }
        let label = Memory::parse(&spec).label();
        by_label
            .entry(label)
            .or_insert_with(|| (spec, Vec::new()))
            .1
            .extend(ids);
    }
    Ok(by_label.into_values().collect())
}

/// The sentence for agents an older funes bound to different memories, with its cure.
fn disagreement(groups: &[(String, Vec<String>)]) -> String {
    let parts: Vec<String> = groups
        .iter()
        .enumerate()
        .map(|(i, (spec, ids))| {
            if i == 0 {
                format!("{} bound to {spec}", are(ids))
            } else {
                format!("{} to {spec}", ids.join(", "))
            }
        })
        .collect();
    format!(
        "{}; this host reads local until `funes bind <memory>` binds it to one",
        parts.join(", ")
    )
}

/// `ids`, as the subject of "bound": "claude is", "claude, codex are".
fn are(ids: &[String]) -> String {
    format!("{} {}", ids.join(", "), if ids.len() == 1 { "is" } else { "are" })
}

/// Validate an explicitly-named memory: fine if it exists or the Hub is unreachable; offer to create
/// it if missing (default **no**, to catch typos). Returns whether it created the repo.
async fn ensure_remote_exists(remote: &str) -> Result<bool> {
    let target = Memory::parse(remote);
    let Memory::Remote { uri } = &target else {
        return Ok(false); // a local path — nothing to check on the Hub
    };
    match memory::remote_reachability(uri).await {
        Reachability::Ok | Reachability::Offline => Ok(false),
        Reachability::Missing => {
            let (owner, name, _) = hub::parse_hf(uri)?;
            if std::io::stdin().is_terminal()
                && confirm(
                    &format!("{remote} doesn't exist on the Hub. Create it as a private dataset? [y/N] "),
                    false,
                )
            {
                hub::create_dataset_repo(&owner, &name).await?;
                eprintln!("created {owner}/{name} as a private dataset.");
                Ok(true)
            } else {
                Err(target.missing_error())
            }
        }
    }
}

/// The embedding model `memory` records, `None` when it holds no dataset or is unreachable.
/// When the local memory `takes` its model from it, a memory whose model can't be read is an error:
/// a guess could pin the local memory to another model.
pub(crate) async fn bound_model(memory: &str, takes: bool) -> Result<Option<EmbeddingModel>> {
    let target = Memory::parse(memory);
    async {
        let ds = match target.state().await? {
            MemoryState::Ready(ds) => ds,
            // A dataset with no rows still records its model.
            MemoryState::Empty => match target.dataset().await? {
                Some(ds) => ds,
                None => return Ok(None),
            },
            MemoryState::Offline if takes => {
                bail!("it is unreachable, and the local memory takes its embedding model: try again once it is back")
            }
            MemoryState::Offline => {
                eprintln!("note: can't reach {memory} right now — proceeding; it'll be used once it's back.");
                return Ok(None);
            }
            MemoryState::Unauthorized if takes => return Err(target.unauthorized_error()),
            _ => return Ok(None),
        };
        dataset::embedding_model(&ds).map(Some)
    }
    .await
    .with_context(|| format!("can't read {memory}"))
}

/// Whether `bind` and `add` owe `remote` a first push. The push hook can't make one: the
/// wrong-memory guard refuses a first publish off a terminal. One is owed when funes just `created`
/// the repo, when this host never pushed there (no receipt), or when the repo holds no dataset
/// (emptied since the last push). Otherwise the next push takes the pending turns. A memory at a
/// local path is never pushed to.
pub async fn owes_first_push(remote: &str, created: bool) -> Result<bool> {
    let target = Memory::parse(remote);
    let Memory::Remote { uri } = &target else {
        return Ok(false);
    };
    if created || !receipt::held_before(uri) {
        return Ok(true);
    }
    Ok(matches!(target.state().await?, MemoryState::Empty))
}

/// The first push to `remote`, at a terminal. The wrong-memory guard asks before publishing to a
/// memory holding none of this host's chunks, except a repo funes just `created`. Errors that
/// aren't fatal to the install (a read-only token, held-back secrets) are reported without failing
/// the command.
pub(crate) async fn first_push(remote: &str, created: bool) -> Result<()> {
    let confirm = if created {
        push::Confirm::Yes
    } else {
        push::Confirm::Ask(push::prompt_new_memory)
    };
    // Boxed: the push's state machine nested in the add's overflows rustc's layout recursion.
    match Box::pin(push::run_push(Memory::parse(remote), false, confirm, &[])).await {
        Ok(pushed) => {
            print!("{}", pushed.report);
            Ok(())
        }
        Err(e) if hub::is_read_only(&e) => {
            eprintln!("{}.", push::read_only_error(remote));
            Ok(())
        }
        Err(e) => Err(e),
    }
}
