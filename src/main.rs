use std::{
    collections::{BTreeMap, HashSet},
    process::exit,
};

use clap::Parser;
use regex::Regex;
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
struct Cli {
    revision: String,
    #[clap(long = "job-type-re")]
    job_type_name_regex: Option<Regex>,
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let Cli {
        revision,
        job_type_name_regex,
    } = Cli::parse();

    let client = reqwest::Client::new();

    let treeherder_host = "https://treeherder.mozilla.org";

    let revision = client
        .get(format!(
            "{treeherder_host}/api/project/try/push/?revision={revision}"
        ))
        .send()
        .await
        .unwrap()
        .json::<Revision>()
        .await
        .unwrap();

    log::info!("fetching for revision(s): {:?}", [&revision]);

    let Revision { meta, mut results } = revision;
    let RevisionMeta { count } = meta;
    assert!(results.len() == usize::try_from(count).unwrap());
    if count > 1 {
        log::warn!("more than one `result` found for specified push");
    } else if count == 0 {
        log::error!("no `results` found; does the push you specified exist?");
        exit(1);
    }
    let RevisionResult { id: push_id } = results.pop().unwrap();

    let Jobs(mut jobs) = client
        .get(format!("{treeherder_host}/api/jobs/?push_id={push_id}"))
        .send()
        .await
        .unwrap()
        .json::<Jobs>()
        .await
        .unwrap();

    jobs.retain(|job| {
        let is_complete = job.state == "completed";
        if !is_complete {
            log::warn!(
                "skipping incomplete job {} (`{}` on `{}` `{}`)",
                job.id,
                job.job_type_name,
                job.platform,
                job.platform_option
            );
        }
        is_complete
    });
    if let Some(job_type_name_regex) = job_type_name_regex {
        jobs.retain(|job| job_type_name_regex.is_match(&job.job_type_name));
    }

    if jobs.is_empty() {
        log::warn!("no jobs selected, exiting");
        return;
    }

    if log::log_enabled!(log::Level::Info) {
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
}
