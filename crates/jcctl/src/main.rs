//! `jcctl`, the reconciler and repository CLI (API/03).
//!
//! `validate`, `plan`, `apply` and `schema export` are implemented. Configuration kinds are
//! read from the repository by the component that serves them, so there is no configuration
//! API to write them to (CC-72): the manifest half of `plan` and `apply` reports what the
//! repository declares, and the live half is the seed entities, replayed into the broker
//! through the Context Gateway `--gateway-url` names (T-0421, CC-50).

use jcctl::commands;
use jcctl::loader::Repository;
use jcctl::model;
use jcctl::platform::InMemory;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "usage: jcctl validate --repo-dir <path> [--project-dir <slug>=<path>]...\n       jcctl validate --project <path> [--slug <slug>] [--org-domain <d>] [--param <name>=<value>]...\n       jcctl plan --repo-dir <path> [--gateway-url <url>] [--token-file <path>] [--json]\n       jcctl apply --repo-dir <path> [--gateway-url <url>] [--token-file <path>] [--prune] [--confirm-deletions]\n       jcctl drift --repo-dir <path> [--gateway-url <url>] [--token-file <path>] [--json] [--adopt-dir <path>]\n       jcctl export --repo-dir <path> --project <slug> --out-dir <path> [--revision <sha>]\n       jcctl import <source> --repo-dir <path> [--namespace <slug>] [--org-domain <d>] [--conflict fail|skip|replace|rename] [--json]\n       jcctl schema export [--out <dir>]\n       jcctl workspace render --repo-dir <path> --prefix <ws-name-> [--out-dir <dir>]\n       jcctl workspace diff --base-dir <checkout of the base> --repo-dir <checkout of the workspace> [--json]\n       jcctl roles render --repo-dir <path>\n       jcctl roles seed --repo-dir <path>\n       jcctl roles input --repo-dir <path> --base-dir <path> --changes <name-status file> --author <login> [--author-email <e>] [--groups a,b]\n       jcctl model generate|diff|validate --repo-dir <path> [--url <url>]\n       jcctl model import <dataModel.Subject/Model> --out <file> [--url <url>]\n       jcctl model infer --file <sample.csv|xlsx|json|pdf> [--url <url>]\n       jcctl pipeline test --pipeline <manifest.yaml> --sample <file> [--format csv|json|text] [--capture <url>]\n       jcctl artifacts rebuild --repo-dir <path> --out-dir <dir> [--space <name>] [--revision <sha>]\n       jcctl sync --repo-dir <path> --source <project>/<name> --checkout <dir> [--state <file>] [--once] [--json]\n       jcctl publish ckan --repo-dir <path> --project <slug> --host <gateway host> [--organization-title <t>] [--api-token-env <VAR>] [--age-key-file <path>] [--withdraw]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();

    match words.as_slice() {
        ["validate", "--repo-dir", dir, rest @ ..] => match project_dirs(rest) {
            Some(dirs) => validate(jcctl::commands::validate::run_assembled(
                Path::new(dir),
                &jcctl::assemble::Directories(dirs),
            )),
            None => usage(),
        },
        ["validate", "--project", dir, rest @ ..] => match project_options(rest) {
            Some((slug, org_domain, values)) => validate(jcctl::commands::validate::run_project(
                Path::new(dir),
                slug.as_deref(),
                &org_domain,
                &values,
            )),
            None => usage(),
        },
        ["plan", "--repo-dir", dir, rest @ ..] => match plan_options(rest) {
            Some((live, as_json)) => plan(Path::new(dir), live, as_json),
            None => usage(),
        },
        ["apply", "--repo-dir", dir, rest @ ..] => match apply_options(rest) {
            Some((options, live)) => apply(Path::new(dir), options, live),
            None => usage(),
        },
        ["drift", "--repo-dir", dir, rest @ ..] => match drift_options(rest) {
            Some((as_json, adopt_dir, live)) => {
                drift(Path::new(dir), as_json, adopt_dir.as_deref(), live)
            }
            None => usage(),
        },
        ["import", source, rest @ ..] => match bundle_options(rest) {
            Some((dir, options, as_json)) => import(Path::new(source), &dir, options, as_json),
            None => usage(),
        },
        ["export", rest @ ..] => match export_options(rest) {
            Some((repo_dir, project, out, revision)) => {
                export(&repo_dir, &project, &out, revision.as_deref())
            }
            None => usage(),
        },
        ["model", "import", id, rest @ ..] => match import_options(rest) {
            Some((out, url)) => model_import(id, &out, url),
            None => usage(),
        },
        ["model", "infer", rest @ ..] => match infer_options(rest) {
            Some((file, url)) => model_infer(&file, url),
            None => usage(),
        },
        ["model", verb, rest @ ..] => match (model_mode(verb), model_options(rest)) {
            (Some(mode), Some((dir, url))) => model(&dir, url, mode),
            _ => usage(),
        },
        ["pipeline", "test", rest @ ..] => match pipeline_test_options(rest) {
            Some(options) => pipeline_test(&options),
            None => usage(),
        },
        ["artifacts", "rebuild", rest @ ..] => match artifacts_options(rest) {
            Some((dir, options)) => artifacts_rebuild(&dir, &options),
            None => usage(),
        },
        ["sync", rest @ ..] => match sync_options(rest) {
            Some((dir, options, as_json)) => sync(&dir, &options, as_json),
            None => usage(),
        },
        ["publish", "ckan", rest @ ..] => match publish_ckan_options(rest) {
            Some(options) => publish_ckan(&options),
            None => usage(),
        },
        ["roles", "seed", "--repo-dir", dir] => match jcctl::taxonomy::seed(Path::new(dir)) {
            Ok(written) if written.is_empty() => {
                println!("every seeded role is already there; nothing written");
                ExitCode::SUCCESS
            }
            Ok(written) => {
                for path in written {
                    println!("wrote {}", path.display());
                }
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("error: {err}");
                ExitCode::FAILURE
            }
        },
        ["roles", "render", "--repo-dir", dir] => match jcctl::roles::render(Path::new(dir)) {
            Ok(written) => {
                for path in written {
                    println!("{}", path.display());
                }
                ExitCode::SUCCESS
            }
            Err(err) => fail(&err.to_string()),
        },
        ["roles", "input", rest @ ..] => match roles_input_options(rest) {
            Some(options) => roles_input(&options),
            None => usage(),
        },
        ["workspace", "render", "--repo-dir", dir, "--prefix", prefix, rest @ ..] => match rest {
            [] => workspace_render(Path::new(dir), prefix, None),
            ["--out-dir", out] => workspace_render(Path::new(dir), prefix, Some(Path::new(out))),
            _ => usage(),
        },
        ["workspace", "diff", "--base-dir", base, "--repo-dir", dir, rest @ ..] => match rest {
            [] => workspace_diff(Path::new(base), Path::new(dir), false),
            ["--json"] => workspace_diff(Path::new(base), Path::new(dir), true),
            _ => usage(),
        },
        ["schema", "export", rest @ ..] => match out_dir(rest) {
            Some(out) => match export_schemas(&out) {
                Ok(count) => {
                    println!("{count} schemas written to {}", out.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("jcctl: {e}");
                    ExitCode::FAILURE
                }
            },
            None => {
                eprintln!("{USAGE}");
                ExitCode::FAILURE
            }
        },
        _ => usage(),
    }
}

/// `artifacts rebuild --repo-dir <path> --out-dir <dir> [--space <name>] [--revision <sha>]`.
fn artifacts_options(rest: &[&str]) -> Option<(PathBuf, jcctl::commands::artifacts::Options)> {
    let (mut repo_dir, mut out_dir, mut space, mut revision) = (None, None, None, None);
    let mut rest = rest;
    while let [flag, value, tail @ ..] = rest {
        match *flag {
            "--repo-dir" => repo_dir = Some(PathBuf::from(value)),
            "--out-dir" => out_dir = Some(PathBuf::from(value)),
            "--space" => space = Some((*value).to_owned()),
            "--revision" => revision = Some((*value).to_owned()),
            _ => return None,
        }
        rest = tail;
    }
    if !rest.is_empty() {
        return None;
    }
    Some((
        repo_dir?,
        jcctl::commands::artifacts::Options {
            out_dir: out_dir?,
            space,
            revision,
        },
    ))
}

/// Re-renders the artifact store from the repository (DM-44). The objects are written to a
/// directory; mirroring them into the bucket is the store client's job, with the scoped
/// credential this command never sees (PF-32).
fn artifacts_rebuild(repo_dir: &Path, options: &jcctl::commands::artifacts::Options) -> ExitCode {
    let report = match jcctl::commands::artifacts::rebuild(repo_dir, options) {
        Ok(report) => report,
        Err(error) => return fail(&error.to_string()),
    };
    for missing in &report.missing {
        eprintln!(
            "jcctl: {missing} is declared and not in the repository; run `jcctl model generate`"
        );
    }
    println!(
        "{} objects written to {}",
        report.written.len(),
        options.out_dir.display()
    );
    ExitCode::SUCCESS
}

/// `sync --repo-dir <path> --source <project>/<name> --checkout <dir> [--state <file>] [--once]
/// [--json]`, in any order after the verb.
fn sync_options(rest: &[&str]) -> Option<(PathBuf, jcctl::commands::sync::Options, bool)> {
    let (mut repo_dir, mut source, mut checkout, mut state) = (None, None, None, None);
    let (mut once, mut as_json) = (false, false);
    let mut rest = rest;
    while let Some((flag, tail)) = rest.split_first() {
        match *flag {
            "--once" => {
                once = true;
                rest = tail;
            }
            "--json" => {
                as_json = true;
                rest = tail;
            }
            "--repo-dir" | "--source" | "--checkout" | "--state" => {
                let (value, tail) = tail.split_first()?;
                match *flag {
                    "--repo-dir" => repo_dir = Some(PathBuf::from(value)),
                    "--source" => source = Some((*value).to_owned()),
                    "--checkout" => checkout = Some(PathBuf::from(value)),
                    _ => state = Some(PathBuf::from(value)),
                }
                rest = tail;
            }
            _ => return None,
        }
    }
    Some((
        repo_dir?,
        jcctl::commands::sync::Options {
            source: source?,
            checkout: checkout?,
            state,
            once,
        },
        as_json,
    ))
}

/// Runs one tick of a `SyncSource`'s loop (MF-28, MF-34). Exit 2 means a proposal is open, the
/// way `plan` and `drift` report pending work.
fn sync(repo_dir: &Path, options: &jcctl::commands::sync::Options, as_json: bool) -> ExitCode {
    let run = match jcctl::commands::sync::run(repo_dir, options) {
        Ok(run) => run,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let report = jcctl::commands::sync::report(&run);
    if as_json {
        match serde_json::to_string_pretty(&report) {
            Ok(text) => println!("{text}"),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        }
    } else if let Some(proposal) = &run.proposal {
        println!(
            "{} at {}: {} file(s) to review",
            proposal.name,
            proposal.revision,
            proposal.files.len()
        );
    } else {
        println!("{:?}: {}", run.phase, run.reason);
    }
    for refused in run.proposal.iter().flat_map(|p| &p.rejected) {
        eprintln!("{refused}");
    }
    if run.proposal.is_some() {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

/// Writes the preview render of `dir` (CC-78): to `out` one file per manifest, else to stdout
/// as one YAML stream.
fn workspace_render(dir: &Path, prefix: &str, out: Option<&Path>) -> ExitCode {
    let files = match commands::workspace::render(dir, prefix) {
        Ok(files) => files,
        Err(err) => return fail(&err.to_string()),
    };
    let Some(out) = out else {
        for (_, yaml) in &files {
            print!("---\n{yaml}");
        }
        return ExitCode::SUCCESS;
    };
    for (name, yaml) in &files {
        let path = out.join(name);
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, yaml));
        if let Err(err) = written {
            return fail(&format!("{}: {err}", path.display()));
        }
    }
    println!("{} manifests rendered to {}", files.len(), out.display());
    ExitCode::SUCCESS
}

/// What the workspace checked out at `dir` changes against its base at `base` (CC-79), as
/// the Portal's compare lists it. Exit 2 when it changes something, like `plan`.
fn workspace_diff(base: &Path, dir: &Path, as_json: bool) -> ExitCode {
    let entries = match commands::workspace::diff_dirs(base, dir) {
        Ok(entries) => entries,
        Err(err) => return fail(&err.to_string()),
    };
    if as_json {
        println!("{}", commands::workspace::to_json(&entries));
    } else if entries.is_empty() {
        println!("the workspace changes nothing");
    } else {
        for entry in &entries {
            println!("{} {}", entry.operation.as_str(), entry.id);
            for field in &entry.fields {
                let show = |value: &Option<serde_json::Value>| {
                    value.as_ref().map_or("-".to_owned(), |v| v.to_string())
                };
                println!(
                    "  {}: {} -> {}",
                    field.path,
                    show(&field.live),
                    show(&field.declared)
                );
            }
        }
    }
    if entries.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

fn usage() -> ExitCode {
    eprintln!("{USAGE}");
    ExitCode::FAILURE
}

/// Reports every invalid manifest under `dir`, one per line (API/03 section 1).
///
/// Exit code 1 covers both an invalid manifest and an unreadable repository: API/03
/// section 3 gives `validate` no separate code for a bad manifest.
/// `--project-dir slug=path`, any number of times: the checkout of each registered project of
/// a layout 2 organization (CC-86).
fn project_dirs(args: &[&str]) -> Option<BTreeMap<String, PathBuf>> {
    let mut dirs = BTreeMap::new();
    let mut rest = args;
    while let ["--project-dir", pair, tail @ ..] = rest {
        let (slug, dir) = pair.split_once('=')?;
        dirs.insert(slug.to_owned(), PathBuf::from(dir));
        rest = tail;
    }
    rest.is_empty().then_some(dirs)
}

/// `--slug`, `--org-domain` and `--param name=value` of `validate --project`, in any order. A
/// value is read as YAML, so `12` is a number and `true` a boolean (CC-88).
type ProjectOptions = (Option<String>, String, BTreeMap<String, serde_json::Value>);

fn project_options(args: &[&str]) -> Option<ProjectOptions> {
    let (mut slug, mut org_domain, mut values) = (None, "example.org".to_owned(), BTreeMap::new());
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        match *flag {
            "--slug" => slug = Some((*value).to_owned()),
            "--org-domain" => org_domain = (*value).to_owned(),
            "--param" => {
                let (name, raw) = value.split_once('=')?;
                values.insert(name.to_owned(), serde_norway::from_str(raw).ok()?);
            }
            _ => return None,
        }
        rest = tail;
    }
    rest.is_empty().then_some((slug, org_domain, values))
}

fn validate(report: jcctl::commands::validate::Report) -> ExitCode {
    for finding in &report.findings {
        eprintln!("{finding}");
    }
    // Not invalid yet, and the repository has to be migrated before it can be (CC-74).
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    if report.is_valid() {
        println!("{} manifests valid", report.checked);
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "{} manifests valid, {} invalid",
            report.checked,
            report.findings.len()
        );
        ExitCode::FAILURE
    }
}

/// Reports what `apply` would do (API/03 section 2 and 3: exit 2 means pending changes).
fn plan(dir: &Path, live: Connection, as_json: bool) -> ExitCode {
    let repo = match Repository::load(dir) {
        Ok(repo) => repo,
        Err(err) => return fail(&err.to_string()),
    };
    let changes = match commands::plan::compute(&repo, &InMemory::new()) {
        Ok(changes) => changes,
        Err(err) => return fail(&err.to_string()),
    };

    if as_json {
        match serde_json::to_string_pretty(&changes.to_json()) {
            Ok(json) => println!("{json}"),
            Err(err) => return fail(&err.to_string()),
        }
    } else {
        print!("{}", changes.render());
    }

    // The live half: the seed entities, which are the only state a component does not read
    // from the repository by itself (CC-72).
    let gateway = match live.open() {
        Ok(gateway) => gateway,
        Err(err) => return fail(&err),
    };
    let Some(gateway) = gateway else {
        eprintln!("jcctl: no --gateway-url, so no seed entity was compared against a platform");
        return exit_of(changes.is_clean());
    };
    let seeds = match commands::seed::plan(dir, &gateway) {
        Ok(report) => report,
        Err(err) => return fail(&err.to_string()),
    };
    if !as_json {
        print!("{}", seeds.render());
    }

    exit_of(changes.is_clean() && seeds.is_clean())
}

/// Success, or 2 for "there is something to do", the code `plan` and `drift` share.
fn exit_of(clean: bool) -> ExitCode {
    if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}

/// Reports what changed on the platform behind Git's back (API/03 section 3, CC-21).
///
/// Exit 2 means drift, the same code `plan` uses for pending changes: a scheduled run is a
/// cron job whose exit code is the alert, and an operator reads the two resolutions per
/// resource (CC-38, UI-26).
fn drift(dir: &Path, as_json: bool, adopt_dir: Option<&Path>, live: Connection) -> ExitCode {
    // Configuration is not compared, and there is nothing to compare it against: every
    // component reads its manifests from the repository (CC-72, T-0421), so the repository is
    // what is running and a manifest cannot drift away from itself. Running the resource
    // comparison against an empty platform — which is the only one that exists — reported every
    // declared resource as `missing` on every run, on a platform where nothing was wrong, and
    // an alert that is always on is one an operator learns to ignore (T-1216).
    // `commands::drift::detect` is kept for the day a live configuration store exists; it is
    // what adoption is built on, and its tests are what keep it ready.
    if adopt_dir.is_some() {
        eprintln!(
            "jcctl: --adopt-dir has nothing to write: a live entity is data, and data is not \
             adopted into the repository"
        );
    }

    // A seed entity the broker no longer holds as declared is drift, and the same comparison
    // `plan` makes says so (CC-21, CC-72). Nothing is written and nothing is adopted.
    let gateway = match live.open() {
        Ok(gateway) => gateway,
        Err(err) => return fail(&err),
    };
    let Some(gateway) = gateway else {
        // Not a clean run: nothing was compared. A scheduled job whose exit code is the alert
        // must not report "no drift" when it could not look (CC-21).
        return fail(
            "drift needs --gateway-url (or JC_GATEWAY_URL) with --token-file: the seed entities \
             are what can drift, and they are read through the space surface",
        );
    };
    let seeds = match commands::seed::plan(dir, &gateway) {
        Ok(seeds) => seeds,
        Err(err) => return fail(&err.to_string()),
    };
    if as_json {
        println!("{}", json(&seeds.to_json()));
    } else {
        print!("{}", seeds.render());
    }

    exit_of(seeds.is_clean())
}

/// Parses `--json` and the optional `--adopt-dir <path>`, in any order.
fn drift_options(args: &[&str]) -> Option<(bool, Option<PathBuf>, Connection)> {
    let (mut as_json, mut adopt_dir) = (false, None);
    let mut live = Connection::default();
    let mut rest = args;
    while let Some((flag, tail)) = rest.split_first() {
        match *flag {
            "--json" => {
                as_json = true;
                rest = tail;
            }
            "--adopt-dir" => match tail.split_first() {
                Some((value, next)) => {
                    adopt_dir = Some(PathBuf::from(value));
                    rest = next;
                }
                None => return None,
            },
            flag => rest = live.take(flag, tail)?,
        }
    }
    Some((as_json, adopt_dir, live))
}

/// Converges the platform and prints the per-resource result (CC-18, CC-20).
fn apply(dir: &Path, options: commands::apply::Options, live: Connection) -> ExitCode {
    let repo = match Repository::load(dir) {
        Ok(repo) => repo,
        Err(err) => return fail(&err.to_string()),
    };
    // Opened before anything is converged: an unreachable gateway or an unreadable token is a
    // run that cannot finish, and finding that out after half a replay helps nobody.
    let gateway = match live.open() {
        Ok(gateway) => gateway,
        Err(err) => return fail(&err),
    };
    let mut platform = InMemory::new();
    let report = match commands::apply::run(&repo, &mut platform, options) {
        Ok(report) => report,
        Err(err) => return fail(&err.to_string()),
    };

    for result in &report.results {
        println!(
            "{:<9} {} {}",
            result.action.as_str().to_lowercase(),
            result.id,
            match &result.outcome {
                commands::apply::Outcome::Applied => "applied".to_owned(),
                commands::apply::Outcome::Unchanged => "unchanged".to_owned(),
                commands::apply::Outcome::Skipped(why) => format!("skipped: {why}"),
                commands::apply::Outcome::Failed(err) => format!("failed: {err}"),
            }
        );
    }
    if let Some(revision) = &report.revision {
        println!("applied revision {revision}");
    }

    let seeds = match &gateway {
        None => {
            eprintln!(
                "jcctl: no --gateway-url, so no seed entity was replayed (CC-72: the \
                 repository is what every component reads)"
            );
            None
        }
        Some(gateway) => match commands::seed::apply(dir, gateway) {
            Ok(seeds) => Some(seeds),
            Err(err) => return fail(&err.to_string()),
        },
    };
    if let Some(seeds) = &seeds {
        print!("{}", seeds.render());
    }

    if report.is_successful() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

/// Renders every DataModel through Model Tools (API/03 section 4, DM-02, DM-19, DM-32).
///
/// `diff` exits 2 the way `plan` does: a committed artifact that no longer matches a fresh
/// rendering is a pending change, and CI reads the code rather than the log.
fn model(dir: &Path, url: Option<String>, mode: model::Mode) -> ExitCode {
    let url = match url.or_else(|| std::env::var(model::URL_ENV).ok()) {
        Some(url) => url,
        None => {
            return fail(&format!(
                "no Model Tools URL: pass --url or set {} (API/03 section 4)",
                model::URL_ENV
            ))
        }
    };
    let tools = model::ModelTools::new(url);
    let report = match model::run(dir, &tools, mode) {
        Ok(report) => report,
        Err(err) => return fail(&err.to_string()),
    };

    println!("{}", json(&report.to_json()));
    if report.failed() > 0 {
        ExitCode::FAILURE
    } else if report.stale() > 0 {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    }
}

/// Imports one Smart Data Models model as LinkML (DM-07..DM-11).
fn model_import(id: &str, out: &Path, url: Option<String>) -> ExitCode {
    let url = match url.or_else(|| std::env::var(model::URL_ENV).ok()) {
        Some(url) => url,
        None => {
            return fail(&format!(
                "no Model Tools URL: pass --url or set {} (API/03 section 4)",
                model::URL_ENV
            ))
        }
    };
    let result = match model::import(&model::ModelTools::new(url), id, out) {
        Ok(result) => result,
        Err(err) => return fail(&err.to_string()),
    };

    let failed = result["errors"].as_array().is_some_and(|e| !e.is_empty());
    println!("{}", json(&result));
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `jcctl model infer --file <sample>`: a draft model from one sample, printed as the JSON
/// answer of Model Tools (DM-54, DM-32). Nothing is written: the draft is for a person or an
/// agent to hand to the editor.
fn model_infer(file: &Path, url: Option<String>) -> ExitCode {
    let url = match url.or_else(|| std::env::var(model::URL_ENV).ok()) {
        Some(url) => url,
        None => {
            return fail(&format!(
                "no Model Tools URL: pass --url or set {} (API/03 section 4)",
                model::URL_ENV
            ))
        }
    };
    let content = match std::fs::read(file) {
        Ok(content) => content,
        Err(err) => return fail(&format!("cannot read {}: {err}", file.display())),
    };
    let name = file
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sample");
    match model::ModelTools::new(url).infer(name, &content) {
        Ok(answer) => {
            println!("{}", json(&answer));
            ExitCode::SUCCESS
        }
        Err(err) => fail(&err.to_string()),
    }
}

/// What `jcctl publish ckan` was asked to do.
struct PublishCkanOptions {
    repo_dir: PathBuf,
    project: String,
    host: String,
    organization_title: Option<String>,
    api_token_env: Option<String>,
    age_key_file: Option<PathBuf>,
    withdraw: bool,
}

/// Parses the options of `publish ckan`, in any order; `--repo` is accepted for
/// `--repo-dir`. The host is taken without a scheme or a trailing slash, so a URL pasted
/// in place of a host still names the host.
fn publish_ckan_options(args: &[&str]) -> Option<PublishCkanOptions> {
    let mut options = PublishCkanOptions {
        repo_dir: PathBuf::new(),
        project: String::new(),
        host: String::new(),
        organization_title: None,
        api_token_env: None,
        age_key_file: None,
        withdraw: false,
    };
    let mut rest = args;
    while let Some((flag, tail)) = rest.split_first() {
        match (*flag, tail) {
            ("--repo-dir" | "--repo", [dir, tail @ ..]) => {
                options.repo_dir = PathBuf::from(dir);
                rest = tail;
            }
            ("--project", [project, tail @ ..]) => {
                options.project = (*project).to_owned();
                rest = tail;
            }
            ("--host", [host, tail @ ..]) => {
                let host = host
                    .strip_prefix("https://")
                    .or_else(|| host.strip_prefix("http://"))
                    .unwrap_or(host);
                options.host = host.trim_end_matches('/').to_owned();
                rest = tail;
            }
            ("--organization-title", [title, tail @ ..]) => {
                options.organization_title = Some((*title).to_owned());
                rest = tail;
            }
            ("--api-token-env", [variable, tail @ ..]) => {
                options.api_token_env = Some((*variable).to_owned());
                rest = tail;
            }
            ("--age-key-file", [path, tail @ ..]) => {
                options.age_key_file = Some(PathBuf::from(path));
                rest = tail;
            }
            ("--withdraw", tail) => {
                options.withdraw = true;
                rest = tail;
            }
            _ => return None,
        }
    }
    let complete = options.repo_dir.as_os_str().is_empty()
        || options.project.is_empty()
        || options.host.is_empty();
    (!complete).then_some(options)
}

/// Publishes every Endpoint of one project that declares `spec.publish.ckan` (T-0487,
/// EP-62…EP-67, CC-18).
///
/// One line per Endpoint; an Endpoint that fails is reported on stderr and the run goes
/// on to the next, so one unreachable record does not hold the other datasets back. Exit
/// 1 when any Endpoint failed. A second run over an unchanged repository prints
/// `unchanged` for every dataset and writes nothing (CC-18).
fn publish_ckan(options: &PublishCkanOptions) -> ExitCode {
    use jcctl::commands::publish_ckan::{self as command, TokenSource};
    use jcctl::publish::ckan::Settings;
    use jcctl::publish::ckan_http::HttpCkan;

    let repo = match Repository::load(&options.repo_dir) {
        Ok(repo) => repo,
        Err(err) => return fail(&err.to_string()),
    };
    let targets = match command::targets(&repo, &options.project) {
        Ok(targets) => targets,
        Err(err) => return fail(&err.to_string()),
    };
    if targets.is_empty() {
        println!(
            "no endpoint of project {} declares spec.publish.ckan",
            options.project
        );
        return ExitCode::SUCCESS;
    }
    let mut settings = Settings::new(options.host.clone());
    if let Some(title) = &options.organization_title {
        settings = settings.titled(title.clone());
    }
    let source = TokenSource {
        env: options.api_token_env.as_deref(),
        age_key_file: options.age_key_file.as_deref(),
    };

    // One client per instance, built when the first Endpoint of that instance comes up;
    // an instance whose token cannot be read fails every Endpoint that names it.
    let mut clients: std::collections::BTreeMap<String, Result<HttpCkan, String>> =
        std::collections::BTreeMap::new();
    let mut failed = 0usize;
    for target in &targets {
        let client = clients
            .entry(target.instance_name.clone())
            .or_insert_with(|| {
                command::token(&target.instance, repo.root(), source)
                    .map_err(|e| e.to_string())
                    .and_then(|token| {
                        HttpCkan::new(target.instance.base_url(), token).map_err(|e| e.to_string())
                    })
            });
        let api = match client {
            Ok(api) => api,
            Err(message) => {
                eprintln!("{}: {message}", target.id);
                failed += 1;
                continue;
            }
        };
        let line = if options.withdraw {
            command::withdraw_one(api, target)
        } else {
            command::record(target, &settings).and_then(|record| {
                let rows = command::rows(target, &settings)?;
                command::publish_one(api, target, &record, rows.as_deref(), &settings)
            })
        };
        match line {
            Ok(line) => println!("{line}"),
            Err(err) => {
                eprintln!("{}: {err}", target.id);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        eprintln!(
            "{failed} of {} endpoints failed; the others are as reported",
            targets.len()
        );
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The verb after `model`, or `None` for anything else.
fn model_mode(verb: &str) -> Option<model::Mode> {
    match verb {
        "generate" => Some(model::Mode::Generate),
        "diff" => Some(model::Mode::Diff),
        "validate" => Some(model::Mode::Validate),
        _ => None,
    }
}

/// Parses `--repo-dir` and the optional `--url`, in any order.
fn model_options(args: &[&str]) -> Option<(PathBuf, Option<String>)> {
    let (mut dir, mut url) = (None, None);
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        match *flag {
            "--repo-dir" => dir = Some(PathBuf::from(value)),
            "--url" => url = Some((*value).to_owned()),
            _ => return None,
        }
        rest = tail;
    }
    if rest.is_empty() {
        dir.map(|dir| (dir, url))
    } else {
        None
    }
}

/// Parses `--out` and the optional `--url`, in any order.
fn import_options(args: &[&str]) -> Option<(PathBuf, Option<String>)> {
    let (mut out, mut url) = (None, None);
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        match *flag {
            "--out" => out = Some(PathBuf::from(value)),
            "--url" => url = Some((*value).to_owned()),
            _ => return None,
        }
        rest = tail;
    }
    if rest.is_empty() {
        out.map(|out| (out, url))
    } else {
        None
    }
}

/// Parses `--file` and the optional `--url`, in any order.
fn infer_options(args: &[&str]) -> Option<(PathBuf, Option<String>)> {
    let (mut file, mut url) = (None, None);
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        match *flag {
            "--file" => file = Some(PathBuf::from(value)),
            "--url" => url = Some((*value).to_owned()),
            _ => return None,
        }
        rest = tail;
    }
    if rest.is_empty() {
        file.map(|file| (file, url))
    } else {
        None
    }
}

/// One JSON document per run, so a CI step reads the result instead of the log.
fn json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// Parses the two deletion flags, in either order; `None` on anything else (CC-19).
fn apply_options(args: &[&str]) -> Option<(commands::apply::Options, Connection)> {
    let mut options = commands::apply::Options::default();
    let mut live = Connection::default();
    let mut rest = args;
    while let Some((arg, tail)) = rest.split_first() {
        rest = tail;
        match *arg {
            "--prune" => options.prune = true,
            "--confirm-deletions" => options.confirm_deletions = true,
            _ => rest = live.take(arg, rest)?,
        }
    }
    Some((options, live))
}

fn plan_options(args: &[&str]) -> Option<(Connection, bool)> {
    let mut live = Connection::default();
    let mut as_json = false;
    let mut rest = args;
    while let Some((arg, tail)) = rest.split_first() {
        rest = tail;
        match *arg {
            "--json" => as_json = true,
            _ => rest = live.take(arg, rest)?,
        }
    }
    Some((live, as_json))
}

/// The address and the identity `plan` and `apply` reach a live platform with (API/03 §2).
///
/// Both come from the command line or the environment and never from a manifest. Neither is
/// resolved here: a run that names no gateway works on the repository alone and says so,
/// rather than comparing against an empty world.
#[derive(Debug, Default)]
struct Connection {
    gateway_url: Option<String>,
    token_file: Option<String>,
}

impl Connection {
    /// Reads one flag and its value, returning what is left of the arguments.
    ///
    /// `None` is an unknown flag, which the caller turns into the usage text rather than
    /// ignoring: a mistyped `--gateway-url` must not silently plan against nothing.
    fn take<'a>(&mut self, arg: &str, rest: &'a [&'a str]) -> Option<&'a [&'a str]> {
        let (value, tail) = rest.split_first()?;
        match arg {
            "--gateway-url" => self.gateway_url = Some((*value).to_owned()),
            "--token-file" => self.token_file = Some((*value).to_owned()),
            _ => return None,
        }
        Some(tail)
    }

    /// The gateway to work against, or `None` when this run is repository-only.
    ///
    /// `JC_GATEWAY_URL` is the Context Gateway this run writes through, and `JC_TOKEN_FILE`
    /// the file holding the reconciler's ServiceAccount token — a path to a secret, projected
    /// by the deployment and never a value in the environment.
    ///
    /// The flags win over the environment, which is what a Job's `JC_GATEWAY_URL` and the
    /// projected `JC_TOKEN_FILE` carry; a gateway named without a token is an error rather
    /// than an anonymous call, because an anonymous call to a space surface is a refusal with
    /// a confusing message (CC-04).
    fn open(&self) -> Result<Option<jcctl::gateway::Gateway>, String> {
        let url = self
            .gateway_url
            .clone()
            .or_else(|| non_empty("JC_GATEWAY_URL"));
        let Some(url) = url else {
            return Ok(None);
        };
        let token_file = self
            .token_file
            .clone()
            .or_else(|| non_empty("JC_TOKEN_FILE"))
            .ok_or_else(|| {
                "--gateway-url needs an identity: --token-file <path>, or JC_TOKEN_FILE, holding                  the reconciler's ServiceAccount token"
                    .to_owned()
            })?;
        let token = jcctl::gateway::Gateway::token_from(Path::new(&token_file))
            .map_err(|e| e.to_string())?;
        jcctl::gateway::Gateway::new(&url, token)
            .map(Some)
            .map_err(|e| e.to_string())
    }
}

/// An environment variable that is set and not blank.
fn non_empty(variable: &str) -> Option<String> {
    std::env::var(variable)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// Imports a bundle into the repository, rewritten for this project (MF-20…MF-24, PF-22).
///
/// Exit 1 on any rejection, and nothing is written then: MF-24 is a gate, so half an import
/// is not a smaller import, it is a repository that no longer validates.
fn import(
    source: &Path,
    repo_dir: &Path,
    options: commands::import::Options,
    as_json: bool,
) -> ExitCode {
    let report = match commands::import::collect(source, repo_dir, &options) {
        Ok(report) => report,
        Err(err) => return fail(&err.to_string()),
    };

    for rejection in &report.rejections {
        eprintln!("jcctl: {rejection}");
    }
    if !report.is_acceptable() {
        eprintln!(
            "jcctl: {} of the bundle refused, nothing written",
            report.rejections.len()
        );
        if as_json {
            println!("{}", json(&report.to_json()));
        }
        return ExitCode::FAILURE;
    }

    let written = match commands::import::write(repo_dir, &report) {
        Ok(written) => written,
        Err(err) => return fail(&err.to_string()),
    };

    if as_json {
        println!("{}", json(&report.to_json()));
    } else {
        for resource in &report.imported {
            println!(
                "{:<9} {}",
                resource.outcome.as_str().to_lowercase(),
                resource.path.display()
            );
        }
        println!("{written} manifests written to {}", repo_dir.display());
    }
    ExitCode::SUCCESS
}

/// Parses `--repo-dir`, `--namespace`, `--org-domain`, `--conflict` and `--json`, in any order.
fn bundle_options(args: &[&str]) -> Option<(PathBuf, commands::import::Options, bool)> {
    let mut dir = None;
    let mut options = commands::import::Options::default();
    let mut as_json = false;
    let mut rest = args;
    while let Some((flag, tail)) = rest.split_first() {
        if *flag == "--json" {
            as_json = true;
            rest = tail;
            continue;
        }
        let (value, next) = tail.split_first()?;
        match *flag {
            "--repo-dir" => dir = Some(PathBuf::from(value)),
            "--namespace" => options.namespace = Some((*value).to_owned()),
            "--org-domain" => options.org_domain = Some((*value).to_owned()),
            "--conflict" => options.conflict = commands::import::Conflict::parse(value)?,
            _ => return None,
        }
        rest = next;
    }
    dir.map(|dir| (dir, options, as_json))
}

/// Copies one context space out of the live platform as a repository (CC-22, MF-16).
///
/// Like `plan` and `apply`, this runs against the in-process platform until the Context
/// Gateway serves the configuration API, so today it exports a fresh installation: an
/// empty space. The cleaning and the redaction are what the command is for and they are
/// exercised by `commands::export` directly.
fn export(repo_dir: &Path, project: &str, out: &Path, revision: Option<&str>) -> ExitCode {
    // The configuration kinds live in Git (CC-72), so the bundle is the checkout's own
    // `projects/{project}/`: manifests without status or secrets, natives byte for byte, and
    // the index that says what it is (MF-16, MF-17, T-0824).
    let bundle = match commands::export::collect_project(repo_dir, project) {
        Ok(bundle) => bundle,
        Err(err) => return fail(&err.to_string()),
    };
    if bundle.resources.is_empty() && bundle.natives.is_empty() {
        return fail(&format!(
            "no project '{project}' in {} (projects/{project}/ holds nothing)",
            repo_dir.display()
        ));
    }
    let revision = revision.unwrap_or("0000000");
    let exported_by = std::env::var("USER").unwrap_or_else(|_| "jcctl".to_owned());
    let written =
        match commands::export::write_bundle(out, project, revision, &exported_by, &bundle) {
            Ok(written) => written,
            Err(err) => return fail(&err.to_string()),
        };

    for redaction in &bundle.redactions {
        eprintln!(
            "jcctl: redacted {redaction} (MF-17: a manifest carries a secretRef, never a secret)"
        );
    }
    println!("{written} files written to {}", out.display());
    ExitCode::SUCCESS
}

/// Parses `--repo-dir`, `--project`, `--out-dir` and the optional `--revision`, in any order.
fn export_options(args: &[&str]) -> Option<(PathBuf, String, PathBuf, Option<String>)> {
    let (mut repo_dir, mut project, mut out, mut revision) = (None, None, None, None);
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        match *flag {
            "--repo-dir" => repo_dir = Some(PathBuf::from(value)),
            "--project" => project = Some((*value).to_owned()),
            "--out-dir" => out = Some(PathBuf::from(value)),
            "--revision" => revision = Some((*value).to_owned()),
            _ => return None,
        }
        rest = tail;
    }
    if !rest.is_empty() {
        return None;
    }
    Some((repo_dir?, project?, out?, revision))
}

struct RolesInputOptions {
    repo_dir: PathBuf,
    base_dir: PathBuf,
    changes: PathBuf,
    author: String,
    author_email: Option<String>,
    groups: Vec<String>,
}

fn roles_input_options(args: &[&str]) -> Option<RolesInputOptions> {
    let mut repo_dir = None;
    let mut base_dir = None;
    let mut changes = None;
    let mut author = None;
    let mut author_email = None;
    let mut groups = Vec::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let value = it.next()?;
        match *flag {
            "--repo-dir" => repo_dir = Some(PathBuf::from(value)),
            "--base-dir" => base_dir = Some(PathBuf::from(value)),
            "--changes" => changes = Some(PathBuf::from(value)),
            "--author" => author = Some((*value).to_owned()),
            "--author-email" => author_email = Some((*value).to_owned()).filter(|e| !e.is_empty()),
            "--groups" => {
                groups = value
                    .split(',')
                    .filter(|g| !g.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            _ => return None,
        }
    }
    Some(RolesInputOptions {
        repo_dir: repo_dir?,
        base_dir: base_dir?,
        changes: changes?,
        author: author?,
        author_email,
        groups,
    })
}

/// Prints the document `policies/roles.rego` evaluates (PF-52).
fn roles_input(options: &RolesInputOptions) -> ExitCode {
    let listing = match std::fs::read_to_string(&options.changes) {
        Ok(listing) => listing,
        Err(err) => return fail(&format!("{}: {err}", options.changes.display())),
    };
    match jcctl::roles::input(
        &options.repo_dir,
        &options.base_dir,
        &listing,
        &options.author,
        options.author_email.as_deref(),
        &options.groups,
    ) {
        Ok(input) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&input).expect("input serializes")
            );
            ExitCode::SUCCESS
        }
        Err(err) => fail(&err.to_string()),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("jcctl: {message}");
    ExitCode::FAILURE
}

/// Parses the single `--out <dir>` option; `None` on anything else.
/// `jcctl pipeline test`: the manifest, the sample file, its format and where the harness posts.
#[derive(Debug, PartialEq, Eq)]
struct PipelineTestOptions {
    pipeline: PathBuf,
    sample: PathBuf,
    format: jcctl::pipeline_test::SampleFormat,
    capture: String,
}

fn pipeline_test_options(args: &[&str]) -> Option<PipelineTestOptions> {
    use jcctl::pipeline_test::SampleFormat;
    let (mut pipeline, mut sample, mut format, mut capture) = (None, None, None, None);
    let mut rest = args;
    while let [flag, value, tail @ ..] = rest {
        match *flag {
            "--pipeline" => pipeline = Some(PathBuf::from(value)),
            "--sample" => sample = Some(PathBuf::from(value)),
            "--format" => {
                format = Some(match *value {
                    "csv" => SampleFormat::Csv,
                    "json" => SampleFormat::Json,
                    "text" => SampleFormat::Text,
                    _ => return None,
                })
            }
            "--capture" => capture = Some((*value).to_owned()),
            _ => return None,
        }
        rest = tail;
    }
    if !rest.is_empty() {
        return None;
    }
    let sample = sample?;
    let format = format.unwrap_or_else(|| match sample.extension().and_then(|e| e.to_str()) {
        Some("csv") => SampleFormat::Csv,
        Some("json") => SampleFormat::Json,
        _ => SampleFormat::Text,
    });
    Some(PipelineTestOptions {
        pipeline: pipeline?,
        sample,
        format,
        capture: capture
            .unwrap_or_else(|| "http://localhost:9090/internal/pipeline-tests/local".to_owned()),
    })
}

/// Prints the harness the runner would be handed (PL-43, MF-38): the Portal creates it as an
/// ephemeral stream and reads the trace back; from the command line the harness is the
/// reviewable artifact, and `bento lint` takes it as it is (PL-03).
fn pipeline_test(options: &PipelineTestOptions) -> ExitCode {
    use jc_core::envelope::ResourceEnvelope;
    use jc_core::kinds::PipelineSpec;
    let manifest = match std::fs::read_to_string(&options.pipeline) {
        Ok(text) => text,
        Err(err) => return fail(&format!("{}: {err}", options.pipeline.display())),
    };
    let envelope = match ResourceEnvelope::<PipelineSpec>::from_yaml(&manifest) {
        Ok(envelope) => envelope,
        Err(err) => return fail(&format!("{}: {err}", options.pipeline.display())),
    };
    if let Err(err) = envelope.validate() {
        return fail(&format!("{}: {err}", options.pipeline.display()));
    }
    let text = match std::fs::read_to_string(&options.sample) {
        Ok(text) => text,
        Err(err) => return fail(&format!("{}: {err}", options.sample.display())),
    };
    let sample = jcctl::pipeline_test::Sample {
        text: Some(text),
        url: None,
        format: options.format,
    };
    match jcctl::pipeline_test::harness(&envelope.spec, &sample, &options.capture) {
        Ok(harness) => match serde_json::to_string_pretty(&harness) {
            Ok(json) => {
                println!("{json}");
                ExitCode::SUCCESS
            }
            Err(err) => fail(&err.to_string()),
        },
        Err(err) => fail(&err.to_string()),
    }
}

fn out_dir(args: &[&str]) -> Option<PathBuf> {
    match args {
        [] => Some(PathBuf::from("schemas/kinds")),
        ["--out", dir] => Some(PathBuf::from(dir)),
        _ => None,
    }
}

/// Writes the draft-07 schema of every catalogued kind to `out/{Kind}.json` (MF-09, CC-12).
fn export_schemas(out: &std::path::Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(out)?;
    for info in jc_core::registry::KINDS {
        let schema =
            jc_core::registry::schema_of(info.kind).expect("every catalogued kind has a schema");
        let mut json = serde_json::to_string_pretty(&schema)?;
        json.push('\n');
        std::fs::write(out.join(format!("{}.json", info.kind)), json)?;
    }
    // The one entity type the platform defines itself (PF-54): data, not a manifest kind, but
    // a schema a writer and a form read the same way.
    let mut kpi = serde_json::to_string_pretty(&jc_core::kpi::schema())?;
    kpi.push('\n');
    std::fs::write(out.join(format!("{}.json", jc_core::kpi::KPI_TYPE)), kpi)?;
    Ok(jc_core::registry::KINDS.len() + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_option_parsing() {
        assert_eq!(
            export_options(&[
                "--repo-dir",
                "/tmp/repo",
                "--project",
                "ovzdusie",
                "--out-dir",
                "/tmp/x"
            ]),
            Some((
                PathBuf::from("/tmp/repo"),
                "ovzdusie".to_owned(),
                PathBuf::from("/tmp/x"),
                None
            ))
        );
        assert_eq!(
            export_options(&[
                "--out-dir",
                "/tmp/x",
                "--revision",
                "3f9c2e1",
                "--project",
                "air",
                "--repo-dir",
                "/tmp/repo"
            ]),
            Some((
                PathBuf::from("/tmp/repo"),
                "air".to_owned(),
                PathBuf::from("/tmp/x"),
                Some("3f9c2e1".to_owned())
            ))
        );
        assert_eq!(
            export_options(&["--repo-dir", "/tmp/repo", "--project", "air"]),
            None,
            "no out-dir"
        );
        assert_eq!(
            export_options(&["--out-dir", "/tmp/x", "--project", "air"]),
            None,
            "no repo-dir"
        );
        assert_eq!(export_options(&["--repo-dir"]), None, "no value");
        assert_eq!(export_options(&["--space", "ovzdusie"]), None, "not a flag");
    }

    #[test]
    fn infer_options_take_the_file_and_an_optional_url() {
        let (file, url) = infer_options(&["--file", "s.csv"]).expect("parses");
        assert_eq!(file, PathBuf::from("s.csv"));
        assert_eq!(url, None);
        let (_, url) =
            infer_options(&["--url", "http://mt:8080", "--file", "s.xlsx"]).expect("parses");
        assert_eq!(url.as_deref(), Some("http://mt:8080"));
        assert!(
            infer_options(&["--url", "http://mt:8080"]).is_none(),
            "the file is required"
        );
        assert!(infer_options(&["--file", "s.csv", "--out", "x"]).is_none());
    }

    #[test]
    fn pipeline_test_options_parsing() {
        use jcctl::pipeline_test::SampleFormat;
        let parsed =
            pipeline_test_options(&["--pipeline", "p.yaml", "--sample", "s.csv"]).expect("parses");
        assert_eq!(
            parsed.format,
            SampleFormat::Csv,
            "the extension picks the format"
        );
        assert!(parsed.capture.starts_with("http://localhost:9090/"));
        let explicit = pipeline_test_options(&[
            "--pipeline",
            "p.yaml",
            "--sample",
            "s.txt",
            "--format",
            "json",
            "--capture",
            "http://portal/c",
        ])
        .expect("parses");
        assert_eq!(explicit.format, SampleFormat::Json);
        assert_eq!(explicit.capture, "http://portal/c");
        assert_eq!(pipeline_test_options(&["--sample", "s.csv"]), None);
        assert_eq!(
            pipeline_test_options(&[
                "--pipeline",
                "p.yaml",
                "--sample",
                "s.csv",
                "--format",
                "xml"
            ]),
            None
        );
        assert_eq!(
            pipeline_test_options(&["--pipeline", "p.yaml", "--sample"]),
            None
        );
    }

    #[test]
    fn out_dir_parsing() {
        assert_eq!(out_dir(&[]), Some(PathBuf::from("schemas/kinds")));
        assert_eq!(out_dir(&["--out", "/tmp/x"]), Some(PathBuf::from("/tmp/x")));
        assert_eq!(out_dir(&["--out"]), None);
        assert_eq!(out_dir(&["--outdir", "x"]), None);
        assert_eq!(out_dir(&["--out", "a", "b"]), None);
    }

    #[test]
    fn export_writes_one_draft07_schema_per_kind() {
        let dir = std::env::temp_dir().join("jcctl-schema-export-test");
        let _ = std::fs::remove_dir_all(&dir);
        let count = export_schemas(&dir).expect("export succeeds");
        // Every registered kind, plus the KeyPerformanceIndicator entity schema (PF-54).
        assert_eq!(count, jc_core::registry::KINDS.len() + 1);
        assert!(dir.join("KeyPerformanceIndicator.json").exists());

        for info in jc_core::registry::KINDS {
            let text = std::fs::read_to_string(dir.join(format!("{}.json", info.kind)))
                .unwrap_or_else(|e| panic!("{}: {e}", info.kind));
            let schema: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
            assert_eq!(
                schema["$schema"],
                serde_json::json!("http://json-schema.org/draft-07/schema#"),
                "{} must be draft-07",
                info.kind
            );
        }
        std::fs::remove_dir_all(&dir).expect("clean up");
    }

    /// T-0421: `plan` and `apply` take one address and one identity, and a flag this binary
    /// does not know is the usage text rather than a run against nothing (API/03 section 2).
    #[test]
    fn the_connection_flags_parse_and_an_unknown_one_does_not() {
        let (live, as_json) =
            plan_options(&["--gateway-url", "http://gw:9090", "--json"]).expect("the flags parse");
        assert_eq!(live.gateway_url.as_deref(), Some("http://gw:9090"));
        assert!(as_json);

        let (options, live) = apply_options(&[
            "--prune",
            "--gateway-url",
            "http://gw:9090",
            "--token-file",
            "/var/run/secrets/token",
        ])
        .expect("the flags parse");
        assert!(options.prune);
        assert_eq!(live.token_file.as_deref(), Some("/var/run/secrets/token"));

        // A flag nobody declared, and a flag whose value is missing: both are the usage text.
        let (_, adopt, live) = drift_options(&[
            "--adopt-dir",
            "/tmp/adopt",
            "--gateway-url",
            "http://gw:9090",
        ])
        .expect("drift takes the same connection");
        assert_eq!(adopt, Some(PathBuf::from("/tmp/adopt")));
        assert_eq!(live.gateway_url.as_deref(), Some("http://gw:9090"));

        assert!(plan_options(&["--gateway"]).is_none());
        assert!(plan_options(&["--gateway-url"]).is_none());
        assert!(apply_options(&["--token-file"]).is_none());
        assert!(apply_options(&["--broker-url", "http://broker:1026"]).is_none());
        assert!(drift_options(&["--adopt-dir"]).is_none());
    }

    /// A run that names no gateway is repository-only rather than a run against an empty
    /// world, and a gateway named without a token is an error rather than an anonymous call.
    #[test]
    fn a_gateway_without_a_token_is_refused_and_no_gateway_is_no_platform() {
        let none = Connection::default();
        assert!(none.open().expect("no gateway is not an error").is_none());

        let named = Connection {
            gateway_url: Some("http://gw:9090".to_owned()),
            token_file: None,
        };
        let refused = named.open().expect_err("a gateway needs an identity");
        assert!(refused.contains("--token-file"), "{refused}");
    }

    /// The fields of a parsed `publish ckan` call, for comparing two parses (T-2534).
    fn ckan(args: &[&str]) -> Option<(PathBuf, String, String, bool)> {
        publish_ckan_options(args).map(|o| (o.repo_dir, o.project, o.host, o.withdraw))
    }

    const CKAN_BASE: [&str; 6] = [
        "--repo-dir",
        "/r",
        "--project",
        "air",
        "--host",
        "ckan.example.org",
    ];

    fn ckan_ok(host: &str, withdraw: bool) -> Option<(PathBuf, String, String, bool)> {
        Some((
            PathBuf::from("/r"),
            "air".to_owned(),
            host.to_owned(),
            withdraw,
        ))
    }

    /// EP-62, CC-18: a flag written last with no value is an incomplete call, not a flag
    /// that silently keeps an earlier value.
    #[test]
    fn a_flag_missing_its_value_at_the_end_of_argv_returns_none() {
        for flag in [
            "--repo-dir",
            "--repo",
            "--project",
            "--host",
            "--organization-title",
            "--api-token-env",
            "--age-key-file",
        ] {
            let mut args = CKAN_BASE.to_vec();
            args.push(flag);
            assert!(ckan(&args).is_none(), "{flag}");
        }
    }

    /// EP-62: a host that is empty, or nothing but a scheme and slashes, names no CKAN.
    #[test]
    fn an_empty_host_value_is_refused_as_incomplete() {
        for host in ["", "https://", "http://", "/", "https:///"] {
            let args = ["--repo-dir", "/r", "--project", "air", "--host", host];
            assert!(ckan(&args).is_none(), "{host:?}");
        }
    }

    /// EP-62: a URL pasted in place of a host still names the host.
    #[test]
    fn a_host_given_as_a_full_https_or_http_url_is_reduced_to_the_bare_host() {
        for host in ["https://ckan.example.org", "http://ckan.example.org"] {
            let args = ["--repo-dir", "/r", "--project", "air", "--host", host];
            assert_eq!(ckan(&args), ckan_ok("ckan.example.org", false), "{host}");
        }
    }

    /// EP-62: a scheme and any number of trailing slashes go together.
    #[test]
    fn a_host_with_a_trailing_slash_and_a_scheme_strips_both() {
        for host in [
            "https://ckan.example.org/",
            "http://ckan.example.org//",
            "ckan.example.org/",
        ] {
            let args = ["--repo-dir", "/r", "--project", "air", "--host", host];
            assert_eq!(ckan(&args), ckan_ok("ckan.example.org", false), "{host}");
        }
    }

    /// EP-62: each of repo dir, project and host is required and must not be empty.
    #[test]
    fn repo_dir_project_and_host_are_all_required() {
        for missing in 0..3 {
            let args: Vec<&str> = CKAN_BASE
                .chunks(2)
                .enumerate()
                .filter(|(i, _)| *i != missing)
                .flat_map(|(_, pair)| pair.iter().copied())
                .collect();
            assert!(ckan(&args).is_none(), "without {}", CKAN_BASE[missing * 2]);
            let mut emptied = CKAN_BASE;
            emptied[missing * 2 + 1] = "";
            assert!(ckan(&emptied).is_none(), "empty {}", CKAN_BASE[missing * 2]);
        }
        assert_eq!(ckan(&CKAN_BASE), ckan_ok("ckan.example.org", false));
    }

    /// EP-62: `--repo` is the same flag as `--repo-dir`.
    #[test]
    fn repo_is_accepted_as_an_alias_for_repo_dir() {
        let mut args = CKAN_BASE;
        args[0] = "--repo";
        assert_eq!(ckan(&args), ckan(&CKAN_BASE));
    }

    /// EP-62: a flag the command does not know is refused, wherever it stands.
    #[test]
    fn an_unknown_flag_returns_none() {
        for unknown in ["--dry-run", "--hosts", "-h", "ckan.example.org", "--HOST"] {
            let mut args = CKAN_BASE.to_vec();
            args.push(unknown);
            assert!(ckan(&args).is_none(), "{unknown}");
            args.insert(0, unknown);
            args.pop();
            assert!(ckan(&args).is_none(), "{unknown} first");
        }
    }

    /// EP-62: `--withdraw` takes no value, so the flag after it is still read as a flag.
    #[test]
    fn withdraw_is_a_bare_flag_that_does_not_consume_the_next_argument() {
        let args = [
            "--withdraw",
            "--repo-dir",
            "/r",
            "--project",
            "air",
            "--host",
            "ckan.example.org",
        ];
        assert_eq!(ckan(&args), ckan_ok("ckan.example.org", true));
        let mut last = CKAN_BASE.to_vec();
        last.push("--withdraw");
        assert_eq!(ckan(&last), ckan_ok("ckan.example.org", true));
    }

    /// EP-62: the order of the flags does not change what they mean.
    #[test]
    fn flags_in_any_order_parse_the_same() {
        let pairs: Vec<[&str; 2]> = CKAN_BASE.chunks(2).map(|p| [p[0], p[1]]).collect();
        for order in [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ] {
            let args: Vec<&str> = order.iter().flat_map(|i| pairs[*i]).collect();
            assert_eq!(ckan(&args), ckan_ok("ckan.example.org", false), "{order:?}");
        }
    }

    /// EP-62: a flag given twice keeps the last value, as the shell reader would expect.
    #[test]
    fn duplicate_host_flags_the_last_one_wins() {
        let mut args = CKAN_BASE.to_vec();
        args.extend(["--host", "https://open.example.org/"]);
        assert_eq!(ckan(&args), ckan_ok("open.example.org", false));
    }

    /// A fresh directory for one `apply` case (T-2533).
    fn apply_dir(name: &str) -> PathBuf {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default();
        let dir =
            std::env::temp_dir().join(format!("jcctl-main-{name}-{}-{now}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        dir
    }

    fn put(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
        std::fs::write(path, text).expect("a file");
    }

    const APPLY_TOKEN: &str = "eyJhbGciOiJSUzI1NiJ9.t2533-reconciler-token";

    /// A repository with one project, one space and one seed entity in it.
    fn seeded_repository(name: &str) -> PathBuf {
        let root = apply_dir(name);
        put(
            &root,
            "org.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Organization\nmetadata:\n  name: bb\n  namespace: org\nspec:\n  domain: bb.sk\n  locales: [\"sk\"]\n  defaultLocale: sk\n",
        );
        put(
            &root,
            "projects/air/project.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: Project\nmetadata:\n  name: air\n  namespace: org\nspec:\n  organizationRef: bb\n",
        );
        put(
            &root,
            "projects/air/spaces/air/space.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: ContextSpace\nmetadata:\n  name: air\n  namespace: air\nspec:\n  isSandbox: false\n",
        );
        put(
            &root,
            "projects/air/spaces/air/entities/seed/stations.json",
            r#"[{"id":"urn:ngsi-ld:AirQualityObserved:bb.sk:air:1","type":"AirQualityObserved","index":{"type":"Property","value":1}}]"#,
        );
        root
    }

    /// A gateway named on the command line, with its token in a file beside the repository.
    fn gateway_connection(url: &str, dir: &Path) -> Connection {
        let token = dir.join("token");
        std::fs::write(&token, format!("{APPLY_TOKEN}\n")).expect("the token file");
        Connection {
            gateway_url: Some(url.to_owned()),
            token_file: Some(token.display().to_string()),
        }
    }

    /// A gateway that answers every GET and every POST with one fixed status and body, and
    /// counts what it was asked.
    fn fixed_gateway(
        get: (u16, &'static str),
        post: (u16, &'static str),
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        let port = listener.local_addr().expect("an address").port();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = std::sync::Arc::clone(&calls);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut raw = Vec::new();
                let mut buffer = [0_u8; 4096];
                while let Ok(read) = stream.read(&mut buffer) {
                    raw.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&raw);
                    let Some((head, body)) = text.split_once("\r\n\r\n") else {
                        if read == 0 {
                            break;
                        }
                        continue;
                    };
                    let length = head
                        .lines()
                        .find_map(|l| {
                            let (n, v) = l.split_once(':')?;
                            n.eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())?
                        })
                        .unwrap_or(0);
                    if body.len() >= length || read == 0 {
                        break;
                    }
                }
                counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let text = String::from_utf8_lossy(&raw);
                let (status, body) = if text.starts_with("POST") { post } else { get };
                let body = body.replace("{authorization}", &format!("Bearer {APPLY_TOKEN}"));
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (format!("http://127.0.0.1:{port}"), calls)
    }

    const CONFIRMED: commands::apply::Options = commands::apply::Options {
        prune: true,
        confirm_deletions: true,
    };

    /// CC-18, CC-20: a repository that does not load is a failed run, not an empty one.
    #[test]
    fn a_repository_that_fails_to_load_exits_failure() {
        let broken = apply_dir("broken");
        put(
            &broken,
            "org.yaml",
            "apiVersion: joinedcontext.com/v1alpha1\nkind: NoSuchKind\nmetadata:\n  name: x\n",
        );
        for dir in [broken, apply_dir("gone").join("not-there")] {
            assert_eq!(
                apply(&dir, CONFIRMED, Connection::default()),
                ExitCode::FAILURE,
                "{}",
                dir.display()
            );
        }
    }

    /// CC-18: nothing declared is nothing to do, and that is a success.
    #[test]
    fn an_empty_repository_applies_zero_resources_and_exits_success() {
        let empty = apply_dir("empty");
        assert_eq!(
            apply(&empty, CONFIRMED, Connection::default()),
            ExitCode::SUCCESS
        );
    }

    /// CC-72: with no gateway the seed is left alone, and the manifests decide the exit.
    #[test]
    fn a_run_with_no_gateway_applies_manifests_and_exits_by_the_manifest_report_alone() {
        let repo = seeded_repository("no-gateway");
        for options in [
            CONFIRMED,
            commands::apply::Options {
                prune: true,
                confirm_deletions: false,
            },
        ] {
            assert_eq!(
                apply(&repo, options, Connection::default()),
                ExitCode::SUCCESS
            );
        }
    }

    /// CC-72: a gateway that cannot be reached, or named without a readable token, fails the
    /// run instead of passing it with the seed silently left out.
    #[test]
    fn an_unreachable_gateway_fails_the_run() {
        let repo = seeded_repository("unreachable");
        let closed = gateway_connection("http://127.0.0.1:1", &repo);
        assert_eq!(apply(&repo, CONFIRMED, closed), ExitCode::FAILURE);
        let no_token = Connection {
            gateway_url: Some("http://127.0.0.1:1".to_owned()),
            token_file: Some(repo.join("no-token-here").display().to_string()),
        };
        assert_eq!(apply(&repo, CONFIRMED, no_token), ExitCode::FAILURE);
    }

    /// CC-18, CC-72: the manifests applied, the seed write was refused: the run failed.
    #[test]
    fn seed_replay_failure_after_a_successful_manifest_apply_still_exits_failure() {
        let (url, calls) =
            fixed_gateway((404, ""), (403, r#"{"title":"no Policy grants create"}"#));
        let repo = seeded_repository("seed-refused");
        let connection = gateway_connection(&url, &repo);
        assert_eq!(apply(&repo, CONFIRMED, connection), ExitCode::FAILURE);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    /// CC-18: a seed the gateway already holds as declared is written again by nobody, and the
    /// run passes.
    #[test]
    fn every_resource_and_seed_unchanged_exits_success() {
        let (url, calls) = fixed_gateway(
            (
                200,
                r#"{"id":"urn:ngsi-ld:AirQualityObserved:bb.sk:air:1","type":"AirQualityObserved","index":{"type":"Property","value":1}}"#,
            ),
            (500, ""),
        );
        let repo = seeded_repository("unchanged");
        let connection = gateway_connection(&url, &repo);
        assert_eq!(apply(&repo, CONFIRMED, connection), ExitCode::SUCCESS);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no write"
        );
    }

    /// CC-04: a 401 fails the run on the first answer; nothing retries a token the gateway
    /// does not accept.
    #[test]
    fn a_gateway_that_answers_401_is_a_named_failure_not_a_retry_loop() {
        let (url, calls) = fixed_gateway((401, r#"{"title":"token expired"}"#), (401, ""));
        let repo = seeded_repository("unauthorized");
        let connection = gateway_connection(&url, &repo);
        assert_eq!(apply(&repo, CONFIRMED, connection), ExitCode::FAILURE);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        let gateway = gateway_connection(&url, &repo)
            .open()
            .expect("opens")
            .expect("a gateway");
        let said = commands::seed::apply(&repo, &gateway)
            .expect_err("refused")
            .to_string();
        assert!(
            said.contains("401") && said.contains("token expired"),
            "{said}"
        );
    }

    /// CC-06: what `apply` prints on a refusal is the error's text; it never carries the
    /// reconciler's token, not even when the gateway's answer repeats the header it was sent.
    #[test]
    fn the_printed_outcome_lines_never_contain_the_reconciler_token_even_on_a_gateway_refusal() {
        for (get, post) in [
            ((401, r#"{"detail":"rejected {authorization}"}"#), (500, "")),
            ((404, ""), (403, "denied for {authorization}")),
        ] {
            let (url, _) = fixed_gateway(get, post);
            let repo = seeded_repository("echo");
            let gateway = gateway_connection(&url, &repo)
                .open()
                .expect("opens")
                .expect("a gateway");
            let said = commands::seed::apply(&repo, &gateway)
                .expect_err("refused")
                .to_string();
            assert!(!said.contains(APPLY_TOKEN), "{said}");
        }
    }
}
