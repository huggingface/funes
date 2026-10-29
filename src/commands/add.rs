//! `funes add <agent>` / `funes remove <agent>`: install funes into a coding agent on the memory
//! this host is bound to, and take it out again. An add bootstraps the one-time steps the
//! integration's hooks can't do unattended: the first index, and the first push when a memory is
//! bound.

use super::bind::{self, Resolved};
use super::index;
use crate::agents::registry;
use crate::hub;
use crate::inference::EmbeddingModel;
use crate::memory::dataset::model;
use crate::memory::{self, Memory, MemoryState, Reachability};
use crate::traces::spool;
use crate::ui::prompt::{ask, confirm};
use anyhow::{bail, Context, Result};
use std::io::IsTerminal;
use std::path::Path;

/// The answers an unattended `funes add` gives up front, for the two questions it would otherwise
/// put to a terminal. They are separate because they grant different things: `yes` agrees to the
/// add's own work, `trust` runs code funes can't vouch for.
#[derive(Clone, Copy, Debug, Default)]
pub struct Unattended {
    /// Index the agent's existing sessions on a first add without asking.
    pub yes: bool,
    /// Run an integration funes can't vouch for without asking.
    pub trust: bool,
}

/// Resolve `id`'s integration and the memory, and run its `setup add`. A refresh is installed only
/// as `setup add` is about to run, so a run that stops short of it — trust declined, a memory that
/// does not resolve, a first index declined — leaves the registry as it was: the agent's hooks run
/// what is installed there.
pub async fn run(id: &str, memory: Option<&str>, from: Option<&str>, unattended: Unattended) -> Result<()> {
    if !spool::is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    let root = registry::default_root()?;
    let refresh = !root.join(id).is_dir() || from.is_some() || registry::refreshes_on_every_run(&root, id);
    let prepared = prepare_agent(id, refresh, from, Some(unattended.trust)).await?;
    // Naming a memory binds the host to it, as `funes bind` does. Naming none, the agent takes
    // the host's.
    let (resolved, binds) = match memory {
        Some(spec) => (bind::resolve_named(spec).await?, true),
        None => adopt_host_memory(&root, id).await?,
    };
    let registry_root = &root;
    let install = |memory: Option<String>| async move {
        let integration = prepared.install(registry_root)?;
        setup_add(&integration, memory.as_deref())?;
        // The note an older funes kept beside the agent is obsolete once its setup ran on the
        // host's memory; left, a later bare add would bind the host to it again.
        registry::bind(registry_root, id, None)
    };
    bootstrap_add(id, resolved, binds, install, unattended.yes).await
}

/// Run `id`'s `setup remove`, then delete its files. Nothing installed is the state `remove`
/// produces, so meeting it is a success, not an unknown agent. The installed copy is what runs —
/// fetched anew only when it speaks a contract this funes cannot run.
pub async fn remove(id: &str) -> Result<()> {
    if !spool::is_id(id) {
        bail!("{id:?} is not an integration id (lowercase [a-z0-9_-])");
    }
    let root = registry::default_root()?;
    if !root.join(id).is_dir() {
        eprintln!("nothing to remove — no {id} integration is installed.");
        registry::discard(&root, id)?;
        return spool::forget_missing(id);
    }
    let integration = prepare_agent(id, registry::speaks_another_contract(&root, id), None, None)
        .await?
        .install(&root)?;
    integration.remove()?;
    registry::discard(&root, id)?;
    // The hooks that asked for the spool are gone with the integration, so the refusals they left
    // must not outlive it: `remove` takes the spool too, which would show them again.
    spool::forget_missing(id)
}

/// Run `integration`'s `setup add`, bound to `memory`.
pub(crate) fn setup_add(integration: &registry::Integration, memory: Option<&str>) -> Result<()> {
    integration.add(memory)?;
    // Whatever an older hook asked for, this install's hooks are the ones that ask now.
    spool::forget_missing(&integration.manifest.id)
}

/// An integration confirmed to run: the copy installed, or a refresh staged to replace it.
pub(crate) enum Prepared {
    Installed(registry::Integration),
    Staged(registry::Provisioned),
}

impl Prepared {
    /// The integration to run, a staged refresh installed first.
    pub(crate) fn install(self, root: &Path) -> Result<registry::Integration> {
        match self {
            Prepared::Installed(integration) => Ok(integration),
            Prepared::Staged(provisioned) => provisioned.install(root),
        }
    }
}

/// Resolve `id`'s integration for a run and confirm it when funes can't vouch for it — all before
/// `add` touches a memory or `remove` runs anything. With `refresh`, its files are fetched and
/// staged, so the script funes executes is the one it just wrote — unless the source holds the
/// release already installed; a source that can't be reached leaves the installed copy to run,
/// unless `from` named it. Without, the installed copy runs as installed. Says what it refreshed
/// the files with, when it did. `trust` is `--trust`'s answer for files funes can't vouch for, `None`
/// from a command that has no such flag.
pub(crate) async fn prepare_agent(
    id: &str,
    refresh: bool,
    from: Option<&str>,
    trust: Option<bool>,
) -> Result<Prepared> {
    let root = registry::default_root()?;
    let installed = root.join(id).is_dir();
    let staged = if refresh {
        match registry::provision(&root, id, from).await {
            Ok(staged) => staged,
            // Another publisher's files are not a refresh the installed copy stands in for.
            Err(e) if e.downcast_ref::<registry::Takeover>().is_some() => return Err(e),
            Err(e) if installed && from.is_none() => {
                // What failed and why, without the layers between: a request error names its URL
                // at every one.
                let why = match e.chain().count() {
                    1 => e.to_string(),
                    _ => format!("{e}: {}", e.root_cause()),
                };
                eprintln!("note: the {id} integration could not be refreshed ({why}) — running the installed copy.");
                None
            }
            Err(e) => {
                return Err(if e.downcast_ref::<registry::Absent>().is_some() {
                    unknown_agent(&root, id, e)
                } else {
                    e
                });
            }
        }
    } else {
        None
    };
    let Some(provisioned) = staged else {
        let provenance = installed_provenance(&root, id)?;
        let integration = registry::open(&root, id)?;
        confirm_trust(&integration.dir, &integration.manifest, &provenance, trust)?;
        return Ok(Prepared::Installed(integration));
    };
    // Files confirmed once, from the same source, unchanged since: confirmed still.
    if !registry::is_recorded(&root, id, &provisioned.origin, &provisioned.files) {
        confirm_trust(&root.join(id), &provisioned.manifest, &provisioned.provenance, trust)?;
    }
    Ok(Prepared::Staged(provisioned))
}

/// Whether funes vouches for the copy installed at `root/<id>`: it does for files it recorded
/// installing and finds as it left them. A changed file, or no record, is confirmed like any
/// other files it did not write.
fn installed_provenance(root: &Path, id: &str) -> Result<registry::Provenance> {
    Ok(match registry::verify_installed(root, id)? {
        registry::Verification::Intact => registry::Provenance::Vouched,
        registry::Verification::Changed(files) => registry::Provenance::Unvouched(format!(
            "the installed copy, whose {} changed since funes installed it",
            files.join(", ")
        )),
        registry::Verification::Unrecorded => {
            registry::Provenance::Unvouched("the installed copy, recorded by nothing".to_string())
        }
    })
}

/// A failed provision for an agent with no files on this machine is usually a typo, so the error
/// names what is installed and where another integration comes from.
fn unknown_agent(root: &Path, id: &str, e: anyhow::Error) -> anyhow::Error {
    let installed = registry::registered_ids(root);
    let listing = if installed.is_empty() {
        "none are installed yet".to_string()
    } else {
        format!("installed: {}", installed.join(", "))
    };
    e.context(format!(
        "no {id} integration on this machine ({listing}) — see docs/add.md for the maintained agents and how to add your own"
    ))
}

/// Confirm before funes executes the integration `manifest` declares from `dir`, when it does not
/// vouch for it, saying what it is — the package by publisher and version — and where it came
/// from. The default is no. `trust` is the answer `--trust` gave up front, and `None` from a command
/// that has no such flag, so the refusal off a terminal suggests the flag only where it is taken.
/// With the flag, the same line names the files and the flag that trusted them, so a log says what
/// was trusted and from where. Trusted, not run: the command can still stop before setup.
pub(crate) fn confirm_trust(
    dir: &Path,
    manifest: &registry::Manifest,
    provenance: &registry::Provenance,
    trust: Option<bool>,
) -> Result<()> {
    let registry::Provenance::Unvouched(origin) = provenance else {
        return Ok(());
    };
    let id = &manifest.id;
    let package = format!(
        "{id}{} by {} ({})",
        manifest.version.as_ref().map(|v| format!(" {v}")).unwrap_or_default(),
        registry::publisher(&manifest.repo),
        manifest.repo
    );
    if trust == Some(true) {
        eprintln!(
            "funes: {}/setup — {package}, from {origin} — trusted by --trust.",
            dir.display()
        );
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        let hint = match trust {
            Some(_) => ", or pass --trust if you have checked it",
            None => "",
        };
        bail!(
            "the {id} integration at {} — {package} — comes from {origin}, which funes can't vouch \
             for — run this in a terminal to confirm it{hint}",
            dir.display()
        );
    }
    if !confirm(
        &format!(
            "funes is about to run {}/setup — {package}, from {origin}. Trust it? [y/N] ",
            dir.display()
        ),
        false,
    ) {
        bail!("{id} not confirmed — its setup was not run");
    }
    Ok(())
}

/// The memory a bare add installs an agent on: the host's. A host from before the binding is
/// bound as an older funes bound its agents first; one whose agents disagree is `funes bind`'s to
/// settle. A local host is offered a memory on the Hub, which the add then binds the host to, as
/// the flag says.
async fn adopt_host_memory(root: &Path, id: &str) -> Result<(Option<Resolved>, bool)> {
    if let Some(disagreement) = bind::migrate_bindings(root, Some(id)).await? {
        bail!("{disagreement}");
    }
    if let Some(memory) = memory::binding()? {
        return Ok((Some(Resolved { memory, created: false }), false));
    }
    let offered = offer_hub_memory().await?;
    let binds = offered.is_some();
    Ok((offered, binds))
}

/// With no memory named, offer to set one up on the Hub — but only when a token is present and we can
/// prompt. Suggests `<user>/funes-memory`: use it if it exists, offer to create it if not. Returns the
/// memory to bind (with whether it was just created), or `None` to stay local.
async fn offer_hub_memory() -> Result<Option<Resolved>> {
    let interactive = std::io::stdin().is_terminal();
    if !hub::has_token() {
        if interactive {
            eprintln!("staying local — set HF_TOKEN (or run `hf auth login`) and re-run `funes add …` to sync across machines or a team.");
        }
        return Ok(None);
    }
    // A scripted (non-interactive) add can't prompt, so it stays local unless a memory was named.
    if !interactive
        || !confirm(
            "Push your memory to a Hugging Face dataset, so it follows you across machines? [Y/n] ",
            true,
        )
    {
        return Ok(None);
    }
    let user = match hub::whoami().await {
        Ok(u) => u,
        Err(e) => {
            eprintln!("couldn't read your Hugging Face identity ({e:#}) — staying local.");
            return Ok(None);
        }
    };
    let memory = format!("{user}/funes-memory");
    let uri = format!("hf://datasets/{memory}");
    match memory::remote_reachability(&uri).await {
        Reachability::Ok => {
            Ok(confirm(&format!("Use your memory {memory}? [Y/n] "), true)
                .then_some(Resolved { memory, created: false }))
        }
        Reachability::Offline => {
            eprintln!("can't reach the Hub right now — staying local; re-run when you're online.");
            Ok(None)
        }
        Reachability::Missing => {
            if confirm(
                &format!("Create {memory} as a private dataset for your memory? [Y/n] "),
                true,
            ) {
                hub::create_dataset_repo(&user, "funes-memory").await?;
                eprintln!("created {memory} as a private dataset.");
                Ok(Some(Resolved { memory, created: true }))
            } else {
                Ok(None)
            }
        }
    }
}

/// `funes add <agent> [memory]`: bootstrap the one-time steps the hooks can't do unattended,
/// around the integration's own `setup add` (converts the agent's history into its spool,
/// registers hooks + MCP).
///
/// 1. on a first add (no rows in the local memory yet), ask — the first index is about a minute of
///    work, and declining aborts the add before anything is installed, so nothing is wired up.
///    `yes` is the answer given up front. Off a terminal there is no one to ask, so a first add
///    without it is an error naming the flag: a script never takes an add that wired nothing up
///    for one that worked;
/// 2. bind the host when the memory was named or offered, every other agent following — once the
///    add is going ahead, so a declined one leaves the host as it was;
/// 3. `install` — writes the spool, bakes the memory in;
/// 4. build the first index from that spool, so recall and the push have content;
/// 5. first push if a memory is bound — clears the overlap guard so the push hook works thereafter.
async fn bootstrap_add<F, Fut>(
    agent: &str,
    resolved: Option<Resolved>,
    binds: bool,
    install: F,
    yes: bool,
) -> Result<()>
where
    F: FnOnce(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let local = index::local_memory_model().await?;
    let bound = match &resolved {
        Some(Resolved { memory, .. }) => bind::bound_model(memory, local.is_none())
            .await?
            .map(|model| (memory.as_str(), model)),
        None => None,
    };
    if let (Some((memory, there)), Some(here)) = (bound, local) {
        if there != here {
            bail!(
                "{memory} is embedded with {}, and the local memory with {}: a memory takes one embedding model",
                there.id(),
                here.id()
            );
        }
    }
    let first_add = !matches!(Memory::local().state().await?, MemoryState::Ready(_));
    if first_add {
        if yes {
            eprintln!("funes: indexing your existing {agent} sessions, if any, so recall works (about a minute).");
        } else if !std::io::stdin().is_terminal() {
            bail!(
                "funes add {agent} indexes your existing {agent} sessions first and asks before it does, \
                 but there was no one to answer — run it at a terminal, or pass --yes"
            );
        } else if !confirm(
            &format!("funes will index your existing {agent} sessions, if any, so recall works (about a minute). Proceed? [Y/n] "),
            true,
        ) {
            eprintln!("funes: skipped — nothing was wired up. Run `funes add {agent}` again when you're ready.");
            return Ok(());
        }
    }
    if local.is_none() {
        let model = match bound {
            Some((memory, model)) => {
                eprintln!(
                    "funes: embedding the local memory with {}, the model of {memory}.",
                    model.id()
                );
                model
            }
            None if std::io::stdin().is_terminal() => ask_model(host_model()),
            None => {
                let model = host_model();
                eprintln!(
                    "funes: embedding the local memory with {}, this host's default off a terminal.",
                    model.id()
                );
                model
            }
        };
        index::ensure_local_memory(model).await?;
    }
    if binds {
        bind::bind_host(resolved.clone(), Some(agent)).await?;
    }
    install(resolved.as_ref().map(|r| r.memory.clone())).await?;
    if first_add {
        seed_local_index(agent).await;
    }
    // A memory at a local path is read where it is: push only publishes to the Hub.
    if let Some(Resolved { memory, created }) = resolved {
        if matches!(Memory::parse(&memory), Memory::Remote { .. }) {
            // The integration is in place by now; only the push is owed, and it takes a terminal.
            bind::first_push(&memory, created).await.with_context(|| {
                format!(
                    "funes is added to {agent}, bound to {memory}, but the first push there did not go \
                     through — run `funes push {memory}` at a terminal to publish there"
                )
            })?;
        }
    }
    Ok(())
}

/// Build the first index from the spool `agent`'s setup has just converted its history into,
/// `$FUNES_HOME/spool/<agent>`. An empty or absent spool and a build error are notes, not
/// failures: the hooks are in, and they drain whatever lands there.
async fn seed_local_index(agent: &str) {
    let spool = spool::spool_dir(agent);
    let has_sessions = std::fs::read_dir(&spool).is_ok_and(|mut entries| entries.next().is_some());
    if !has_sessions {
        eprintln!("funes: no {agent} sessions to index yet.");
        return;
    }
    eprintln!("funes: indexing your recent {agent} sessions…");
    // Boxed: the index's state machine nested in the add's overflows rustc's layout recursion.
    if let Err(e) = Box::pin(index::run_index_seed(&spool)).await {
        eprintln!(
            "funes: initial index didn't complete ({e:#}) — the hooks are installed; run `funes index` to build it."
        );
    }
}

/// Which embedding model the local memory is created with, asked at a terminal; `default` when
/// the answer names neither.
fn ask_model(default: EmbeddingModel) -> EmbeddingModel {
    let choices = if default == model::MULTILINGUAL {
        "[e/M]"
    } else {
        "[E/m]"
    };
    ask(&format!(
        "Index your sessions as English, or multilingual for other languages too? {choices} "
    ))
    .map_or(default, |answer| parse_model(&answer, default))
}

fn parse_model(input: &str, default: EmbeddingModel) -> EmbeddingModel {
    match input.trim().to_ascii_lowercase().as_str() {
        "e" | "english" => model::ENGLISH,
        "m" | "multilingual" => model::MULTILINGUAL,
        _ => default,
    }
}

/// The model this host's locale suggests: English for an English or unset locale, multilingual
/// for any other language.
fn host_model() -> EmbeddingModel {
    let locale = ["LC_ALL", "LC_MESSAGES", "LANG"]
        .into_iter()
        .find_map(|var| std::env::var(var).ok().filter(|value| !value.is_empty()));
    model_for_locale(locale.as_deref())
}

fn model_for_locale(locale: Option<&str>) -> EmbeddingModel {
    let language = locale.and_then(|l| l.split(['_', '.', '@']).next()).unwrap_or_default();
    match language.to_ascii_lowercase().as_str() {
        "" | "c" | "posix" | "en" => model::ENGLISH,
        _ => model::MULTILINGUAL,
    }
}

#[cfg(test)]
mod tests {
    use super::{model_for_locale, parse_model};
    use crate::memory::dataset::model::{ENGLISH, MULTILINGUAL};

    #[test]
    fn an_answer_names_the_model_and_anything_else_takes_the_default() {
        for default in [ENGLISH, MULTILINGUAL] {
            for answer in ["\n", "nope"] {
                assert_eq!(parse_model(answer, default), default, "{answer:?}");
            }
            for answer in ["e", "English\n"] {
                assert_eq!(parse_model(answer, default), ENGLISH, "{answer:?}");
            }
            for answer in ["m\n", " M ", "multilingual"] {
                assert_eq!(parse_model(answer, default), MULTILINGUAL, "{answer:?}");
            }
        }
    }

    #[test]
    fn a_locale_in_another_language_than_english_defaults_to_multilingual() {
        for locale in [
            None,
            Some("C"),
            Some("C.UTF-8"),
            Some("POSIX"),
            Some("en_US.UTF-8"),
            Some("en"),
        ] {
            assert_eq!(model_for_locale(locale), ENGLISH, "{locale:?}");
        }
        for locale in ["zh_CN.UTF-8", "fr_FR", "ja_JP.eucJP", "de_DE@euro", "pt"] {
            assert_eq!(model_for_locale(Some(locale)), MULTILINGUAL, "{locale}");
        }
    }
}
