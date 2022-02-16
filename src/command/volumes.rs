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

use ansi_term::Colour::Yellow;
use clap::{Arg, Command as ClapCommand};
use k8s_openapi::{api::core::v1 as api, apimachinery::pkg::api::resource::Quantity};

use crate::{
    command::command_def::{exec_match, show_arg, sort_arg, start_clap, Cmd},
    command::{run_list_command, Extractor},
    completer,
    env::Env,
    kobj::{KObj, ObjType},
    output::ClickWriter,
    table::CellSpec,
};

use std::array::IntoIter;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;

lazy_static! {
    static ref PV_EXTRACTORS: HashMap<String, Extractor<api::PersistentVolume>> = {
        let mut m: HashMap<String, Extractor<api::PersistentVolume>> = HashMap::new();
        m.insert("Capacity".to_owned(), volume_capacity);
        m.insert("Access Modes".to_owned(), volume_access_modes);
        m.insert("Reclaim Policy".to_owned(), volume_reclaim_policy);
        m.insert("Status".to_owned(), volume_status);
        m.insert("Claim".to_owned(), volume_claim);
        m.insert("Storage Class".to_owned(), volume_storage_class);
        m.insert("Reason".to_owned(), volume_reason);
        m.insert("Volume Mode".to_owned(), volume_mode);
        m
    };
}

const COL_MAP: &[(&str, &str)] = &[
    ("name", "Name"),
    ("age", "Age"),
    ("capacity", "Capacity"),
    ("accessmodes", "Access Modes"),
    ("replacepolicy", "Reclaim Policy"),
    ("status", "Status"),
    ("cliam", "Claim"),
    ("storageclass", "Storage Class"),
    ("reason", "Reason"),
];

const COL_FLAGS: &[&str] = &{ extract_first!(COL_MAP) };

const EXTRA_COL_MAP: &[(&str, &str)] = &[("labels", "Labels"), ("volumemode", "Volume Mode")];

const EXTRA_COL_FLAGS: &[&str] = &{ extract_first!(EXTRA_COL_MAP) };

fn pv_to_kobj(volume: &api::PersistentVolume) -> KObj {
    let meta = &volume.metadata;
    KObj {
        name: meta.name.clone().unwrap_or_else(|| "<Unknown>".into()),
        namespace: meta.namespace.clone(),
        typ: ObjType::PersistentVolume,
    }
}

fn volume_capacity(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume.spec.as_ref().and_then(|spec| {
        spec.capacity.get("storage").as_ref().map(|q| {
            let quant = Quantity(q.0.clone());
            quant.into()
        })
    })
}

fn volume_access_modes(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume.spec.as_ref().map(|spec| {
        spec.access_modes
            .iter()
            .map(|mode| match mode.as_str() {
                "ReadWriteOnce" => "RWO",
                "ReadOnlyMany" => "ROX",
                "ReadWriteMany" => "RWX",
                "ReadWriteOncePod" => "RWOP",
                _ => "Unknown",
            })
            .collect::<Vec<&str>>()
            .join(", ")
            .into()
    })
}

fn volume_reclaim_policy(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume.spec.as_ref().and_then(|spec| {
        spec.persistent_volume_reclaim_policy
            .as_ref()
            .map(|p| p.clone().into())
    })
}

fn volume_mode(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume
        .spec
        .as_ref()
        .and_then(|spec| spec.volume_mode.as_ref().map(|mode| mode.as_str().into()))
}

fn volume_status(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume
        .status
        .as_ref()
        .and_then(|stat| stat.phase.as_ref().map(|p| p.as_str().into()))
}

fn volume_claim(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume
        .spec
        .as_ref()
        .map(|spec| match spec.claim_ref.as_ref() {
            Some(claim_ref) => {
                let mut claim = claim_ref.namespace.clone().unwrap_or_else(|| "".into());
                claim.push('/');
                claim.push_str(claim_ref.name.as_deref().unwrap_or(""));
                claim.into()
            }
            None => "".into(),
        })
}

fn volume_storage_class(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume.spec.as_ref().and_then(|spec| {
        spec.storage_class_name
            .as_ref()
            .map(|sc| sc.as_str().into())
    })
}

fn volume_reason(volume: &api::PersistentVolume) -> Option<CellSpec<'_>> {
    volume
        .status
        .as_ref()
        .map(|stat| match stat.reason.as_ref() {
            Some(r) => r.as_str().into(),
            None => "".into(),
        })
}

list_command!(
    PersistentVolumes,
    "persistentvolumes",
    "Get persistent volumes in current context",
    super::COL_FLAGS,
    super::EXTRA_COL_FLAGS,
    |clap: ClapCommand<'static>| {
        clap.arg(
            Arg::new("regex")
                .short('r')
                .long("regex")
                .help("Filter pvs by the specified regex")
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
    vec!["persistentvolumes", "pvs"],
    noop_complete!(),
    no_named_complete!(),
    |matches, env, writer| {
        let (request, _response_body) =
            api::PersistentVolume::list_persistent_volume(Default::default())?;
        let cols: Vec<&str> = COL_MAP.iter().map(|(_, col)| *col).collect();
        run_list_command(
            matches,
            env,
            writer,
            cols,
            request,
            COL_MAP,
            Some(EXTRA_COL_MAP),
            Some(&PV_EXTRACTORS),
            pv_to_kobj,
        )
    }
);
