use std::{
    collections::{BTreeMap, HashSet},
    fs,
    num::NonZeroU8,
    path::PathBuf,
    process::exit,
    str::FromStr,
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use clap::Parser;
use futures::stream::StreamExt;
use indicatif::ProgressBar;
use joinery::JoinableIterator as _;
use lazy_format::lazy_format;
use regex::Regex;
use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Deserialize)]
struct Revision {
    meta: RevisionMeta,
    results: Vec<RevisionResult>,
}

#[derive(Debug, Deserialize)]
struct RevisionMeta {
    count: u32,
}

#[derive(Debug, Deserialize)]
struct RevisionResult {
    id: u32,
}

#[derive(Debug)]
struct Jobs(Vec<Job>);

impl<'de> serde::de::Deserialize<'de> for Jobs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::de::Deserializer<'de>,
    {
        #[derive(Debug, Deserialize)]
        struct JobsArena {
            job_property_names: Vec<String>,
            results: Vec<Vec<Value>>,
        }

        let JobsArena {
            job_property_names,
            results,
        } = Deserialize::deserialize(deserializer)?;

        assert!(results
            .iter()
            .all(|res| res.len() == job_property_names.len()));

        macro_rules! assemble_jobs_from_properties {
            (
                $($prop:ident),*
                $(,)?
            ) => {
                struct PropIndices {
                    $($prop: usize,)*
                }
                let indices = PropIndices {
                    $(
                        $prop: job_property_names
                            .iter()
                            .position(|name| name == stringify!($prop))
                            .expect(concat!(
                                "failed to extract field `",
                                stringify!($prop),
                                "`"
                            )),
                    )*
                };

                #[allow(unused_assignments)]
                let jobs = results.into_iter().map(|result| {
                    let mut prop_vals = result.into_iter();
                    let mut iter_idx = 0usize;
                    $(
                        let prop_val =
                            prop_vals.nth(indices.$prop - iter_idx).expect(concat!(
                                "failed to get field `",
                                stringify!($prop),
                                "` from value"
                            ));
                        let $prop = ::serde_json::from_value(prop_val).expect(concat!(
                            "failed to deserialize field `",
                            stringify!($prop),
                            "`"
                        ));
                        iter_idx = indices.$prop.checked_add(1).unwrap();
                    )*
                    Job {
                        $($prop,)*
                    }
                }).collect::<Vec<_>>();

                Ok(Jobs(jobs))
                // OPT: Optimal way to do this is likely to identify the next `nth` offset
            };
        }
        assemble_jobs_from_properties! {
            id,
            job_group_symbol,
            job_type_name,
            job_type_symbol,
            platform,
            result,
            state,
            task_id,
            platform_option,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Job {
    id: u32,
    job_group_symbol: String,
    job_type_name: String,
    job_type_symbol: String,
    platform: String,
    result: String,
    state: String,
    task_id: String,
    platform_option: String,
}

#[derive(Debug, Parser)]
#[clap(about, version)]
struct Cli {
    #[clap(flatten)]
    options: Options,
    #[clap(value_parser = RevisionRef::from_str)]
    revisions: Vec<RevisionRef>,
}

#[derive(Debug, Parser)]
struct Options {
    #[clap(long)]
    out_dir: PathBuf,
    #[clap(long = "job-type-re")]
    job_type_name_regex: Option<Regex>,
    #[clap(long = "artifact")]
    artifact_names: Vec<String>,
    #[clap(long = "max-parallel", default_value = "10")]
    max_parallel_artifact_downloads: NonZeroU8,
    #[clap(long, default_value = "https://treeherder.mozilla.org")]
    treeherder_host: Url,
    #[clap(long, default_value = "https://firefox-ci-tc.services.mozilla.com")]
    taskcluster_host: Url,
}

#[derive(Clone, Debug)]
struct RevisionRef {
    project: String,
    hash: String,
}

impl FromStr for RevisionRef {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.split_once(':')
            .map(|(project, hash)| Self {
                project: project.to_owned(),
                hash: hash.to_owned(),
            })
            .ok_or({
                "no dividing colon found; expected revision ref. of the form <project>:<hash>"
            })
    }
}

#[tokio::main]
async fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();

    let Cli { options, revisions } = Cli::parse();

    static APP_USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

    let client = Client::builder()
        .gzip(true)
        .user_agent(APP_USER_AGENT)
        .build()
        .unwrap();

    for rev_ref in revisions {
        get_artifacts_for_revision(&client, &options, &rev_ref).await
    }
}

async fn get_artifacts_for_revision(client: &Client, options: &Options, revision: &RevisionRef) {
    let Options {
        out_dir,
        job_type_name_regex,
        artifact_names,
        max_parallel_artifact_downloads,
        treeherder_host,
        taskcluster_host,
    } = options;
    let RevisionRef {
        project: project_name,
        hash: revision,
    } = revision;

    log::info!("fetching for revision(s): {:?}", [&revision]);

    let Revision {
        meta: RevisionMeta { count },
        mut results,
    } = client
        .get(format!(
            "{treeherder_host}api/project/{project_name}/push/?revision={revision}"
        ))
        .send()
        .await
        .unwrap()
        .json::<Revision>()
        .await
        .unwrap();

    assert!(results.len() == usize::try_from(count).unwrap());
    if count > 1 {
        log::warn!("more than one `result` found for specified push");
    } else if count == 0 {
        log::error!("no `results` found; does the push you specified exist?");
        exit(1);
    }
    let RevisionResult { id: push_id } = results.pop().unwrap();

    let Jobs(mut jobs) = client
        .get(format!("{treeherder_host}api/jobs/?push_id={push_id}"))
        .send()
        .await
        .unwrap()
        .json::<Jobs>()
        .await
        .unwrap();

    if let Some(job_type_name_regex) = job_type_name_regex {
        jobs.retain(|job| job_type_name_regex.is_match(&job.job_type_name));
    }

    if jobs.is_empty() {
        log::warn!("no jobs selected, exiting");
        return;
    }

    if log::log_enabled!(log::Level::Info) {
        // OPT: We might not need to do this with only `INFO` enabled.
        #[allow(clippy::unwrap_or_default)]
        let summarized_job_tree = jobs.iter().fold(BTreeMap::new(), |mut acc, job| {
            acc.entry(job.platform.clone())
                .or_insert_with(BTreeMap::new)
                .entry(job.platform_option.clone())
                .or_insert_with(BTreeMap::new)
                .entry(&job.job_group_symbol)
                .or_insert_with(BTreeMap::new)
                .entry(&job.job_type_symbol)
                .or_insert_with(Vec::new)
                .push(&job.result);
            acc
        });
        log::debug!("summarized job tree: {summarized_job_tree:#?}");
        log::info!(
            "selected {} jobs across {} platform/option combos",
            jobs.len(),
            jobs.iter()
                .map(|job| (&job.platform, &job.platform_option))
                .collect::<HashSet<_>>()
                .len(),
        );
    }

    let artifacts_len = u64::try_from(jobs.len())
        .expect("number of jobs exceeds `u64::MAX` (!?)")
        .checked_mul(
            artifact_names
                .len()
                .try_into()
                .expect("number of artifacts exceeds `u64::MAX` (!?)"),
        )
        .expect("number of job-artifact combos exceeds `u64::MAX` (!?)");
    let artifacts = tokio_stream::iter(jobs.iter())
        .flat_map(|job| tokio_stream::iter(artifact_names.iter().map(move |name| (job, name))));

    let progress_bar = ProgressBar::new(artifacts_len);
    progress_bar.tick(); // Force the progress bar to show now, rather than waiting until first
                         // completion of a download.

    let run_counts = Arc::new(Mutex::new(BTreeMap::new()));
    let artifacts = artifacts.then(|(job, artifact_name)| {
        let client = &client;
        let out_dir = &out_dir;
        let run_counts = &run_counts;
        let taskcluster_host = &taskcluster_host;
        let progress_bar = progress_bar.clone();
        async move {
            let Job {
                job_group_symbol,
                job_type_symbol,
                job_type_name,
                platform,
                task_id,
                platform_option,
                id,
                ..
            } = job;

            let this_run_idx: u32;
            {
                let mut run_counts = run_counts.lock().unwrap();
                let run_count = run_counts.entry(task_id.clone()).or_insert(0);
                this_run_idx = *run_count;
                *run_count += 1;
            }

            let job_display = lazy_format!(
                "job {}, task {} (`{}`, index {})",
                id,
                task_id,
                job_type_name,
                this_run_idx,
            );

            let is_complete = job.state == "completed";
            if !is_complete {
                if log::log_enabled!(log::Level::Warn) {
                    progress_bar.suspend(|| {
                        log::warn!("skipping incomplete {job_display}");
                    });
                }
                return;
            }

            let skip_log_level = match &*job.result {
                "success" | "testfailed" => {
                    // We might have still hit a timeout, but we expect to still have some
                    // artifacts available.
                    None
                }
                "retry" | "usercancel" => Some(log::Level::Debug),
                "exception" => Some(log::Level::Warn),
                _ => Some(log::Level::Warn),
            };
            if let Some(level) = skip_log_level {
                if log::log_enabled!(level) {
                    progress_bar.suspend(|| {
                        log::log!(level, "skipping `{}` {job_display}", job.result);
                    });
                }
                return;
            }

            let local_artifact_path = {
                let segments: &[&dyn std::fmt::Display] = &[
                    revision,
                    platform,
                    platform_option,
                    job_group_symbol,
                    job_type_symbol,
                    task_id,
                    &this_run_idx,
                    artifact_name,
                ];
                out_dir.join(segments.iter().join_with('/').to_string())
            };

            if local_artifact_path.is_file() {
                if log::log_enabled!(log::Level::Debug) {
                    progress_bar.suspend(|| {
                        log::debug!(
                            "skipping file that already appears to be downloaded: {}",
                            local_artifact_path.display()
                        );
                    });
                }
                return;
            }

            let artifact = match get_artifact(
                client,
                taskcluster_host,
                task_id,
                artifact_name,
                this_run_idx,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(code) => {
                    progress_bar.suspend(|| {
                        log::error!(
                            concat!(
                                "got unexpected response {} with request for {}, ",
                                "artifact {:?}; skipping download"
                            ),
                            code,
                            job_display,
                            artifact_name
                        );
                    });
                    return;
                }
            };

            {
                let parent_dir = local_artifact_path.parent().unwrap();
                fs::create_dir_all(parent_dir)
                    .unwrap_or_else(|e| panic!("failed to create `{}`: {e}", parent_dir.display()));
            }
            fs::write(&local_artifact_path, artifact).unwrap_or_else(|e| {
                panic!(
                    "failed to write artifact `{}`: {e}",
                    local_artifact_path.display()
                )
            });
        }
    });
    let max_parallel_artifact_downloads = usize::from(max_parallel_artifact_downloads.get());
    artifacts
        .for_each_concurrent(max_parallel_artifact_downloads, |()| async {
            progress_bar.inc(1)
        })
        .await;
}

async fn get_artifact(
    client: &Client,
    taskcluster_host: &Url,
    task_id: &str,
    artifact_name: &str,
    run_idx: u32,
) -> Result<Bytes, StatusCode> {
    let url = format!(
        "{taskcluster_host}api/queue/v1/task/{task_id}/runs/{run_idx}/artifacts/{artifact_name}"
    );

    let request = client.get(url);
    log::debug!("sending request {request:?}");

    let response = request.send().await.unwrap();
    {
        let code = response.status();
        if code == StatusCode::OK {
            Ok(response.bytes().await.unwrap())
        } else {
            Err(code)
        }
    }
}
