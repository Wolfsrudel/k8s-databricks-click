// Copyright 2021 Databricks, Inc.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at

// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use clap::{Arg, Command as ClapCommand};
use k8s_openapi::api::core::v1 as api;
use k8s_openapi::ListOptional;

use crate::{
    command::command_def::{exec_match, show_arg, sort_arg, start_clap, Cmd},
    command::{run_list_command, Extractor},
    completer,
    env::{Env, ObjectSelection},
    error::ClickError,
    kobj::{KObj, ObjType},
    output::ClickWriter,
    styles::Styles,
    table::{CellSpec, ColorType},
};

use std::collections::HashMap;
use std::io::Write;
use std::{cell::RefCell, collections::BTreeMap};

lazy_static! {
    static ref POD_EXTRACTORS: HashMap<String, Extractor<api::Pod>> = {
        let mut m: HashMap<String, Extractor<api::Pod>> = HashMap::new();
        m.insert("IP".to_owned(), pod_ip);
        m.insert("Last Restart".to_owned(), last_restart);
        m.insert("Node".to_owned(), pod_node);
        m.insert("Nominated Node".to_owned(), pod_nominated_node);
        m.insert("Readiness Gates".to_owned(), pod_readiness_gates);
        m.insert("Ready".to_owned(), ready_counts);
        m.insert("Restarts".to_owned(), restart_count);
        m.insert("Status".to_owned(), pod_status);
        m
    };
}

const COL_MAP: &[(&str, &str)] = &[
    ("name", "Name"),
    ("ready", "Ready"),
    ("status", "Status"),
    ("restarts", "Restarts"),
    ("age", "Age"),
];

const COL_FLAGS: &[&str] = &{ extract_first!(COL_MAP) };

const EXTRA_COL_MAP: &[(&str, &str)] = &[
    ("ip", "IP"),
    ("labels", "Labels"),
    ("lastrestart", "Last Restart"),
    ("namespace", "Namespace"),
    ("node", "Node"),
    ("nominatednode", "Nominated Node"),
    ("readinessgates", "Readiness Gates"),
];

const EXTRA_COL_FLAGS: &[&str] = &{ extract_first!(EXTRA_COL_MAP) };

fn pod_to_kobj(pod: &api::Pod) -> KObj {
    let containers = match &pod.spec {
        Some(spec) => spec
            .containers
            .iter()
            .map(|cont| cont.name.clone())
            .collect(),
        None => vec![],
    };
    let meta = &pod.metadata;
    KObj {
        name: meta.name.clone().unwrap_or_else(|| "<Unknown>".into()),
        namespace: meta.namespace.clone(),
        typ: ObjType::Pod { containers },
    }
}

// Check if a pod has a waiting container
fn has_waiting(pod: &api::Pod) -> bool {
    match pod
        .status
        .as_ref()
        .and_then(|stat| stat.container_statuses.as_ref())
    {
        Some(stats) => {
            stats.iter().any(|cs| {
                match cs.state.as_ref() {
                    Some(state) => {
                        state.waiting.is_some()
                            || (
                                // if all 3 are None, default is waiting
                                state.running.is_none() && state.terminated.is_none()
                            )
                    }
                    None => false,
                }
            })
        }
        None => false,
    }
}

fn phase_style_color(phase: &str) -> ColorType {
    match phase {
        "Running" | "Active" => ColorType::Success,
        "Terminated" | "Terminating" => ColorType::Danger,
        "Pending" | "ContainerCreating" => ColorType::Warn,
        "Succeeded" => ColorType::Info,
        "Failed" => ColorType::Danger,
        "Unknown" => ColorType::Danger,
        _ => ColorType::Danger,
    }
}

fn pod_ip(pod: &api::Pod) -> Option<CellSpec<'_>> {
    pod.status
        .as_ref()
        .and_then(|status| status.pod_ip.as_ref().map(|pi| pi.as_str().into()))
}

fn pod_node(pod: &api::Pod) -> Option<CellSpec<'_>> {
    pod.spec
        .as_ref()
        .and_then(|spec| spec.node_name.as_ref().map(|nn| nn.as_str().into()))
}

fn pod_nominated_node(pod: &api::Pod) -> Option<CellSpec<'_>> {
    pod.status
        .as_ref()
        .map(|status| match status.nominated_node_name.as_ref() {
            Some(nn) => nn.as_str().into(),
            None => "<none>".into(),
        })
}

// get the number of ready containers and total containers as ready/total
fn ready_counts(pod: &api::Pod) -> Option<CellSpec<'_>> {
    pod.status.as_ref().map(|stat| {
        let mut count = 0;
        let mut ready = 0;
        if let Some(container_statuses) = &stat.container_statuses {
            for cs in container_statuses.iter() {
                count += 1;
                if cs.ready {
                    ready += 1;
                }
            }
        }
        format!("{ready}/{count}").into()
    })
}

fn pod_readiness_gates(pod: &api::Pod) -> Option<CellSpec<'_>> {
    pod.spec.as_ref().and_then(|spec| {
        spec.readiness_gates.as_ref().map(|readiness_gates| {
            if readiness_gates.is_empty() {
                "<none>".into()
            } else {
                let gates: Vec<&str> = readiness_gates
                    .iter()
                    .map(|rg| rg.condition_type.as_str())
                    .collect();
                gates.join(", ").into()
            }
        })
    })
}

fn restart_count(pod: &api::Pod) -> Option<CellSpec<'_>> {
    pod.status.as_ref().and_then(|stat| {
        stat.container_statuses.as_ref().map(|container_statuses| {
            let count = container_statuses
                .iter()
                .fold(0, |acc, cs| acc + cs.restart_count);
            count.into()
        })
    })
}

fn last_restart(pod: &api::Pod) -> Option<CellSpec<'_>> {
    pod.status.as_ref().and_then(|stat| {
        let mut last_restart = None;
        if let Some(container_statuses) = &stat.container_statuses {
            for cs in container_statuses.iter() {
                // TODO: Simplify if https://rust-lang.github.io/rfcs/2497-if-let-chains.html happens
                if let Some(state) = &cs.last_state {
                    if let Some(terminated) = &state.terminated {
                        if let Some(finished) = &terminated.finished_at {
                            if last_restart.is_none() {
                                last_restart = Some(finished.0)
                            } else {
                                // check which is more recent
                                let last = last_restart.unwrap(); // okay: checked above
                                if finished.0 > last {
                                    last_restart = Some(finished.0)
                                }
                            }
                        }
                    }
                }
            }
        }
        last_restart.map(|lr| lr.into())
    })
}

fn pod_status(pod: &api::Pod) -> Option<CellSpec<'_>> {
    let status = if pod.metadata.deletion_timestamp.is_some() {
        // Was deleted
        "Terminating"
    } else if has_waiting(pod) {
        "ContainerCreating"
    } else {
        pod.status
            .as_ref()
            .and_then(|stat| stat.phase.as_deref())
            .unwrap_or("Unknown")
    };
    let fg = phase_style_color(status);
    Some(CellSpec::with_colors(status.into(), Some(fg.into()), None))
}

list_command!(
    Pods,
    "pods",
    "Get pods (in current namespace if set)",
    super::COL_FLAGS,
    super::EXTRA_COL_FLAGS,
    |clap: ClapCommand<'static>| {
        clap.arg(
            Arg::new("labels")
                .short('L')
                .long("labels")
                .help("include labels in output (deprecated, use --show labels)")
                .takes_value(false),
        )
        .arg(
            Arg::new("label")
                .short('l')
                .long("label")
                .help("Get pods with specified label selector (example: app=nginx)")
                .takes_value(true),
        )
        .arg(
            Arg::new("node")
                .short('n')
                .long("node")
                .help("Only fetch pods on the specified node.")
                .takes_value(true),
        )
        .arg(
            Arg::new("regex")
                .short('r')
                .long("regex")
                .help("Filter returned value by the specified regex")
                .takes_value(true),
        )
        .arg(show_arg(EXTRA_COL_FLAGS, true))
        .arg(sort_arg(COL_FLAGS, Some(EXTRA_COL_FLAGS)))
        .arg(
            Arg::new("reverse")
                .short('R')
                .long("reverse")
                .help("Reverse the order of the returned list")
                .takes_value(false),
        )
    },
    vec!["pods"],
    noop_complete!(),
    [].into_iter(),
    |matches, env, writer| {
        let mut opts: ListOptional = ListOptional::<'_> {
            label_selector: matches.get_one::<String>("label").map(|s| s.as_str()),
            ..Default::default()
        };
        let mut field_sel = None;
        match matches.get_one::<String>("node").map(|s| s.as_str()) {
            Some(nodeval) => {
                field_sel = Some(format!("spec.nodeName={nodeval}"));
            }
            None => {
                if let ObjectSelection::Single(obj) = env.current_selection() {
                    if obj.is(ObjType::Node) {
                        field_sel = Some(format!("spec.nodeName={}", obj.name()));
                    }
                }
            }
        }
        opts.field_selector = field_sel.as_deref();

        let (request, _response_body) = match &env.namespace {
            Some(ns) => api::Pod::list_namespaced_pod(ns, opts)?,
            None => api::Pod::list_pod_for_all_namespaces(opts)?,
        };

        let cols: Vec<&str> = COL_MAP.iter().map(|(_, col)| *col).collect();

        run_list_command(
            matches,
            env,
            writer,
            cols,
            request,
            COL_MAP,
            Some(EXTRA_COL_MAP),
            Some(&POD_EXTRACTORS),
            pod_to_kobj,
        )
    }
);

// also add a command to print all the containers of a pod
command!(
    Containers,
    "containers",
    "Print information about the containers of the active pod",
    |clap: ClapCommand<'static>| {
        clap.arg(
            Arg::new("volumes")
                .short('v')
                .long("volumes")
                .help("show information about each containers volume mounts")
                .takes_value(false),
        )
    },
    vec!["conts", "containers"],
    noop_complete!(),
    no_named_complete!(),
    |matches, env, writer| {
        env.apply_to_selection(
            writer,
            Some(&env.click_config.range_separator),
            |obj, writer| {
                if obj.is_pod() {
                    print_containers(obj, env, matches.contains_id("volumes"), writer)
                } else {
                    Err(ClickError::CommandError(
                        "containers only possible on a Pod".to_string(),
                    ))
                }
            },
        )
    }
);

// conainer helper commands
fn print_containers(
    obj: &KObj,
    env: &Env,
    volumes: bool,
    writer: &mut ClickWriter,
) -> Result<(), ClickError> {
    let (request, _) = api::Pod::read_namespaced_pod(
        obj.name(),
        obj.namespace.as_ref().unwrap(),
        Default::default(),
    )?;
    match env
        .run_on_context(|c| {
            c.read::<api::ReadNamespacedPodResponse>(env.get_impersonate_user(), request)
        })
        .unwrap()
    {
        api::ReadNamespacedPodResponse::Ok(pod) => match pod
            .status
            .and_then(|status| status.container_statuses)
        {
            Some(container_statuses) => {
                for cont in container_statuses.iter() {
                    clickwrite!(writer, "Name:\t{}\n", env.styles.bold(cont.name.as_str()));
                    clickwrite!(
                        writer,
                        "  ID:\t\t{}\n",
                        cont.container_id.as_deref().unwrap_or("<none>")
                    );
                    clickwrite!(writer, "  Image:\t{}\n", cont.image_id);
                    print_state_string(&cont.state, &env.styles, writer);
                    clickwrite!(writer, "  Ready:\t{}\n", cont.ready);
                    clickwrite!(writer, "  Restarts:\t{}\n", cont.restart_count);

                    // find the spec for this container
                    if let Some(spec) = pod.spec.as_ref() {
                        let cont_spec = spec.containers.iter().find(|cs| cs.name == cont.name);
                        if let Some(cont_spec) = cont_spec {
                            // print resources
                            clickwrite!(writer, "  Resources:\n");
                            match cont_spec.resources.as_ref() {
                                Some(resources) => {
                                    clickwrite!(writer, "    Requests:\n");
                                    let empty = BTreeMap::new();
                                    let requests = resources.requests.as_ref().unwrap_or(&empty);
                                    for (resource, quant) in requests.iter() {
                                        clickwrite!(writer, "      {}:\t{}\n", resource, quant.0)
                                    }
                                    if requests.is_empty() {
                                        clickwrite!(writer, "      <none>\n");
                                    }
                                    clickwrite!(writer, "    Limits:\n");
                                    let limits = resources.limits.as_ref().unwrap_or(&empty);
                                    for (resource, quant) in limits.iter() {
                                        clickwrite!(writer, "      {}:\t{}\n", resource, quant.0)
                                    }
                                    if limits.is_empty() {
                                        clickwrite!(writer, "      <none>\n");
                                    }
                                }
                                None => {
                                    clickwrite!(writer, "    <Unknown>\n");
                                }
                            }

                            if volumes {
                                // print volumes
                                clickwrite!(writer, "  Volumes:\n");
                                let empty = vec![];
                                let volume_mounts =
                                    cont_spec.volume_mounts.as_ref().unwrap_or(&empty);
                                if !volume_mounts.is_empty() {
                                    for vol in volume_mounts.iter() {
                                        clickwrite!(writer, "   {}\n", vol.name);
                                        clickwrite!(writer, "    Path:\t{}\n", vol.mount_path);
                                        clickwrite!(
                                            writer,
                                            "    Sub-Path:\t{}\n",
                                            vol.sub_path.as_deref().unwrap_or("<none>")
                                        );
                                        clickwrite!(
                                            writer,
                                            "    Read-Only:\t{}\n",
                                            vol.read_only.unwrap_or(false)
                                        );
                                    }
                                } else {
                                    clickwrite!(writer, "    No Volumes\n");
                                }
                            }
                        }
                    }

                    clickwrite!(writer, "\n");
                }
                Ok(())
            }
            None => Err(ClickError::CommandError(
                "No container info returned from api server".to_string(),
            )),
        },
        api::ReadNamespacedPodResponse::Other(o) => Err(ClickError::CommandError(format!(
            "Error getting pod info: {o:?}"
        ))),
    }
}

fn print_state_string(
    state: &Option<api::ContainerState>,
    styles: &Styles,
    writer: &mut ClickWriter,
) {
    clickwrite!(writer, "  State:\t");
    match state {
        Some(state) => {
            if let Some(running) = state.running.as_ref() {
                clickwrite!(writer, "{}\n", styles.success("Running"));
                match &running.started_at {
                    Some(start) => clickwrite!(writer, "\t\t  started at: {}\n", start.0),
                    None => clickwrite!(writer, "\t\t  since unknown\n"),
                }
            } else if let Some(terminated) = state.terminated.as_ref() {
                let message = terminated.message.as_deref().unwrap_or("no message");
                let reason = terminated.reason.as_deref().unwrap_or("no reason");
                let tsr = terminated
                    .finished_at
                    .as_ref()
                    .map(|fa| fa.0.to_string())
                    .unwrap_or_else(|| "<unknown>".to_string());
                clickwrite!(writer, "{}\n", styles.danger("Terminated"));
                clickwrite!(writer, "\t\t  at: {}\n", tsr);
                clickwrite!(writer, "\t\t  code: {}\n", terminated.exit_code);
                clickwrite!(writer, "\t\t  message: {}\n", message);
                clickwrite!(writer, "\t\t  reason: {}\n", reason);
            } else if let Some(waiting) = state.waiting.as_ref() {
                let message = waiting.message.as_deref().unwrap_or("no message");
                let reason = waiting.reason.as_deref().unwrap_or("no reason");
                clickwrite!(writer, "{}\n", styles.warning("Waiting"));
                clickwrite!(writer, "\t\t  message: {}\n", message);
                clickwrite!(writer, "\t\t  reason: {}\n", reason);
            } else {
                clickwrite!(
                    writer,
                    "{}",
                    format!("{} (reason unknown)\n", styles.warning("Waiting"))
                );
            }
        }
        None => clickwrite!(writer, "Unknown"),
    }
}
