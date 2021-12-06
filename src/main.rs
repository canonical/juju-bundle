//! Juju plugin for interacting with a bundle

use std::collections::{HashMap, HashSet};
use std::io::ErrorKind as IoErrorKind;
use std::path::PathBuf;
use std::process::Command;

use ex::fs;
use failure::{format_err, Error};
use petgraph::{
    dot::{Config as GraphConfig, Dot},
    Graph,
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use rayon::ThreadPoolBuilder;
use structopt::{self, clap::AppSettings, StructOpt};
use tempfile::{NamedTempFile, TempDir};

use juju::bundle::{Application, Bundle};
use juju::charm_source::CharmSource;
use juju::cmd::run;

// Helper function for parsing `key=value` pairs passed in on the CLI
fn parse_key_val(s: &str) -> Result<(String, Option<String>), Box<dyn std::error::Error>> {
    let pos = s.find('=');

    match pos {
        Some(pos) => Ok((s[..pos].into(), Some(s[pos + 1..].into()))),
        None => Ok((s.into(), None)),
    }
}

// Helper function for ensure user has hasn't specified invalid values
fn ensure_subset(subset: &HashSet<&String>, superset: &HashSet<&String>) -> Result<(), Error> {
    // Make sure user hasn't passed in any invalid application names
    let diff: Vec<_> = subset
        .difference(superset)
        .into_iter()
        .cloned()
        .map(String::as_ref)
        .collect();

    if diff.is_empty() {
        Ok(())
    } else {
        Err(format_err!("Apps not found in bundle: {}", diff.join(", ")))
    }
}

/// CLI arguments for the `build` subcommand.
#[derive(StructOpt, Debug)]
struct BuildConfig {
    #[structopt(long = "app")]
    #[structopt(parse(try_from_str = parse_key_val))]
    #[structopt(help = "If specified, only these apps will be built")]
    apps: Vec<(String, Option<String>)>,

    #[structopt(short = "b", long = "bundle", default_value = "bundle.yaml")]
    #[structopt(help = "The bundle file to build")]
    bundle: String,

    #[structopt(
        short = "o",
        long = "output-bundle",
        default_value = "built-bundle.yaml"
    )]
    #[structopt(help = "Path where the built bundle.yaml should be written to")]
    output_bundle: String,

    #[structopt(long = "destructive-mode")]
    #[structopt(help = "Build charmcraft charms with `--destructive-mode` flag")]
    destructive_mode: bool,

    #[structopt(long = "serial")]
    #[structopt(help = "Build only one charm at a time")]
    serial: bool,
}

/// CLI arguments for the `deploy` subcommand.
#[derive(StructOpt, Debug)]
struct DeployConfig {
    #[structopt(long = "recreate")]
    #[structopt(help = "Recreate the bundle by ensuring that it's removed before deploying")]
    recreate: bool,

    #[structopt(long = "upgrade-charms")]
    #[structopt(help = "Runs upgrade-charm on each individual charm instead of redeploying")]
    upgrade_charms: bool,

    #[structopt(long = "build")]
    #[structopt(parse(try_from_str = parse_key_val))]
    #[structopt(help = "Build the bundle before deploying it")]
    build: Option<Vec<(String, Option<String>)>>,

    #[structopt(long = "serial")]
    #[structopt(help = "If set, only one charm will be built at a time")]
    serial: bool,

    #[structopt(long = "destructive-mode")]
    #[structopt(help = "Build charmcraft charms with `--destructive-mode` flag")]
    destructive_mode: bool,

    #[structopt(long = "wait", default_value = "60")]
    #[structopt(help = "How long to wait in seconds for model to stabilize before deploying it")]
    wait: u32,

    #[structopt(short = "a", long = "app")]
    #[structopt(help = "Select particular apps to deploy")]
    apps: Vec<String>,

    #[structopt(short = "e", long = "except")]
    #[structopt(help = "Select particular apps to skip deploying")]
    exceptions: Vec<String>,

    #[structopt(short = "b", long = "bundle", default_value = "bundle.yaml")]
    #[structopt(help = "The bundle file to deploy")]
    bundle_path: String,

    #[structopt(name = "deploy-args")]
    #[structopt(help = "Arguments that are collected and passed on to `juju deploy`")]
    deploy_args: Vec<String>,
}

/// CLI arguments for the `remove` subcommand.
#[derive(StructOpt, Debug)]
struct RemoveConfig {
    #[structopt(short = "a", long = "app")]
    #[structopt(help = "Select particular apps to remove")]
    apps: Vec<String>,

    #[structopt(short = "b", long = "bundle", default_value = "bundle.yaml")]
    #[structopt(help = "The bundle file to remove")]
    bundle: String,
}

/// CLI arguments for the `publish` subcommand.
#[derive(StructOpt, Debug)]
struct PublishConfig {
    #[structopt(short = "b", long = "bundle", default_value = "bundle.yaml")]
    #[structopt(help = "The bundle file to publish")]
    bundle_path: String,

    #[structopt(long = "release")]
    #[structopt(help = "Which channels to release to. Can be specified multiple times")]
    #[structopt(default_value = "edge")]
    release_to: Vec<String>,

    #[structopt(long = "serial")]
    #[structopt(help = "If set, only one charm will be built and published at a time")]
    serial: bool,

    #[structopt(long = "prune")]
    #[structopt(
        help = "If set, docker will be pruned between each charm. Enforces --serial also set."
    )]
    prune: bool,

    #[structopt(long = "destructive-mode")]
    #[structopt(help = "Build charmcraft charms with `--destructive-mode` flag")]
    destructive_mode: bool,
}

/// CLI arguments for the `export` subcommand.
#[derive(StructOpt, Debug)]
struct ExportConfig {
    #[structopt(short = "b", long = "bundle", default_value = "bundle.yaml")]
    #[structopt(help = "The bundle file to export")]
    bundle: String,

    #[structopt(short = "o", long = "out")]
    #[structopt(help = "Where to write the exported bundle")]
    out: Option<String>,
}

/// CLI arguments for the `verify` subcommand.
#[derive(StructOpt, Debug)]
struct VerifyConfig {
    #[structopt(short = "b", long = "bundle", default_value = "bundle.yaml")]
    #[structopt(help = "The bundle file to verify")]
    bundle: String,
}

/// Interact with a bundle and the charms contained therein.
#[derive(StructOpt, Debug)]
#[structopt(setting = AppSettings::TrailingVarArg)]
#[structopt(setting = AppSettings::SubcommandRequiredElseHelp)]
enum Config {
    /// Builds a bundle
    ///
    /// Outputs new bundle yaml file pointing at built charms.
    /// If a subset of apps are chosen, bundle relations are only
    /// included if both apps are selected.
    #[structopt(name = "build")]
    Build(BuildConfig),

    /// Deploys a bundle, optionally building and/or recreating it.
    ///
    /// If a subset of apps are chosen, bundle relations are only
    /// included if both apps are selected.
    #[structopt(name = "deploy")]
    Deploy(DeployConfig),

    /// Removes a bundle from the current model.
    ///
    /// If a subset of apps are chosen, bundle relations are only
    /// included if both apps are selected.
    #[structopt(name = "remove")]
    Remove(RemoveConfig),

    /// Publishes a bundle and its charms to the charm store
    ///
    /// Publishes them to the edge channel.
    #[structopt(name = "publish")]
    Publish(PublishConfig),

    /// Exports the bundle to different formats, e.g. graphviz
    #[structopt(name = "export")]
    Export(ExportConfig),

    /// Does as much static verification of the bundle as possible
    #[structopt(name = "verify")]
    Verify(VerifyConfig),
}

/// Run `build` subcommand
fn build(c: BuildConfig) -> Result<(), Error> {
    println!("Building bundle from {}", c.bundle);

    let mut bundle = Bundle::load(c.bundle.clone())?;

    let build_apps = if c.apps.is_empty() {
        None
    } else {
        let apps: HashMap<_, _> = c.apps.into_iter().collect();
        let to_build = apps.keys().into_iter().collect();
        let existing = bundle.applications.keys().into_iter().collect();
        ensure_subset(&to_build, &existing)?;
        Some(apps)
    };

    bundle.build(&c.bundle, build_apps, c.destructive_mode, !c.serial)?;

    bundle.save(&c.output_bundle)?;

    println!("Bundle saved to {}", c.output_bundle);

    Ok(())
}

/// Run `deploy` subcommand
fn deploy(c: DeployConfig) -> Result<(), Error> {
    println!("Building and deploying bundle from {}", c.bundle_path);

    let mut bundle = Bundle::load(&c.bundle_path)?;

    bundle.limit_apps(&c.apps[..], &c.exceptions[..])?;

    // We can have one of three situations:
    //
    //  - No flag passed: Don't build anything / skip calling `bundle.build`
    //  - One plain `--build` flag: Build everything that can be built / call `bundle.build` with `None`
    //  - Multiple apps defined via `--build`: Build only those apps / call `bundle.build` with a HashMap of those apps
    if let Some(build) = c.build {
        let build_apps = if build.is_empty() {
            None
        } else {
            let apps: HashMap<_, _> = build.into_iter().collect();
            let to_build = apps.keys().into_iter().collect();
            let existing = bundle.applications.keys().into_iter().collect();
            ensure_subset(&to_build, &existing)?;
            Some(apps)
        };
        bundle.build(&c.bundle_path, build_apps, c.destructive_mode, !c.serial)?;
    }

    // If we're only upgrading charms, we can skip the rest of the logic
    // that is concerned with tearing down and/or deploying the charms.
    if c.upgrade_charms {
        return Ok(bundle.upgrade_charms()?);
    }

    let temp_bundle = NamedTempFile::new()?;
    bundle.save(temp_bundle.path())?;

    if c.recreate {
        println!("\n\nRemoving bundle before deploy.");
        remove(RemoveConfig {
            apps: c.apps.clone(),
            bundle: c.bundle_path.clone(),
        })?;
    }

    if c.wait > 0 {
        println!("\n\nWaiting for stability before deploying.");

        let exit_status = Command::new("juju")
            .args(&["wait", "-wv", "-t", &c.wait.to_string()])
            .spawn()?
            .wait()?;

        if !exit_status.success() {
            return Err(format_err!(
                "Encountered an error while waiting to deploy: {}",
                exit_status.to_string()
            ));
        }
    }

    println!("\n\nDeploying bundle");

    let exit_status = Command::new("juju")
        .args(&["deploy", &temp_bundle.path().to_string_lossy()])
        .args(c.deploy_args)
        .spawn()?
        .wait()?;

    if !exit_status.success() {
        return Err(format_err!(
            "Encountered an error while deploying bundle: {}",
            exit_status.to_string()
        ));
    }

    Ok(())
}

/// Run `remove` subcommand
fn remove(c: RemoveConfig) -> Result<(), Error> {
    let mut bundle = Bundle::load(c.bundle)?;
    bundle.limit_apps(&c.apps[..], &[])?;
    for name in bundle.applications.keys() {
        Command::new("juju")
            .args(&["remove-application", name])
            .spawn()?
            .wait()?;
    }
    Ok(())
}

/// Run `publish` subcommand
fn publish(c: PublishConfig) -> Result<(), Error> {
    if c.prune && !c.serial {
        return Err(format_err!(
            "To use --prune, you must set the --serial flag as well."
        ));
    }
    let path = c.bundle_path.as_str();
    let bundle = Bundle::load(path)?;

    // Make sure we're logged in first, so that we don't get a bunch of
    // login pages spawn with `charm upload`.
    println!("Ensuring valid credentials with `charmcraft whoami`.");
    run("charmcraft", &["whoami"])?;

    if c.serial {
        ThreadPoolBuilder::new().num_threads(1).build_global()?;
    }

    // Ensure each charm is built and uploaded to each channel
    bundle.applications.par_iter().try_for_each(
        |(name, app): (&String, &Application)| -> Result<(), Error> {
            if app.source(name, path).is_some() {
                app.upload_charmhub(name, path, &c.release_to, c.destructive_mode)?;
            }
            if c.prune {
                run("docker", &["system", "prune", "-af"])?;
            }

            Ok(())
        },
    )?;

    for channel in &c.release_to {
        // Make a copy of the bundle with exact revisions of each charm
        let mut new_bundle = bundle.clone();

        for (name, app) in new_bundle.applications.iter_mut() {
            if app.source(name, path).is_some() {
                app.channel = Some(channel.clone());
                app.source = None;
            }
        }

        // Create a temp dir for the bundle to point `charm` at,
        // since we don't want to modify the existing bundle.yaml file.
        let dir = TempDir::new()?;
        new_bundle.save(&dir.path().join("bundle.yaml"))?;

        // `charm push` expects this file to exist
        fs::copy(
            PathBuf::from(path).with_file_name("README.md"),
            dir.path().join("README.md"),
        )?;

        // Copy `charmcraft.yaml` if it exists
        let copy_result = fs::copy(
            PathBuf::from(path).with_file_name("charmcraft.yaml"),
            dir.path().join("charmcraft.yaml"),
        );

        if let Err(err) = copy_result {
            if err.kind() != IoErrorKind::NotFound {
                return Err(err.into());
            }
        }

        bundle.upload_charmhub(&dir.path().to_string_lossy(), channel)?;
    }

    Ok(())
}

/// Run `export` subcommand
fn export(c: ExportConfig) -> Result<(), Error> {
    let bundle = Bundle::load(&c.bundle)?;

    let mut graph = Graph::<_, String>::new();

    for app in bundle.applications.keys() {
        graph.add_node(app);
    }
    for rel in bundle.relations {
        let app_a = rel[0].split(':').next().unwrap_or(&rel[0]);
        let app_b = rel[1].split(':').next().unwrap_or(&rel[1]);
        let rel_name = rel[0].split(':').last().unwrap_or("");
        let index_a = graph.node_indices().find(|i| graph[*i] == app_a).unwrap();
        let index_b = graph.node_indices().find(|i| graph[*i] == app_b).unwrap();
        graph.add_edge(index_a, index_b, rel_name.to_string());
    }
    let output = Dot::with_config(&graph, &[GraphConfig::EdgeNoLabel]);

    match c.out {
        Some(out) => fs::write(out, format!("{}", output))?,
        None => println!("{}", output),
    }

    Ok(())
}

/// Run `verify` subcommand
fn verify(c: VerifyConfig) -> Result<(), Error> {
    let bundle = Bundle::load(&c.bundle)?;
    println!("Checking {}", c.bundle);

    for (name, app) in bundle.applications {
        if let Some(source) = app.source(&name, &c.bundle) {
            if let Err(err) = CharmSource::load(source) {
                println!("Error for charm {}: {}", name, err);
            }
        }
    }

    Ok(())
}

fn main() -> Result<(), Error> {
    match Config::from_args() {
        Config::Build(c) => build(c),
        Config::Deploy(c) => deploy(c),
        Config::Remove(c) => remove(c),
        Config::Publish(c) => publish(c),
        Config::Export(c) => export(c),
        Config::Verify(c) => verify(c),
    }
}
