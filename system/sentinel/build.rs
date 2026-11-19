// SPDX-License-Identifier: MulanPSL-2.0

use std::{collections::BTreeSet, path::PathBuf};

use robonix_codegen::codegen::{contract_gen, msg_parser, proto_gen};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?);
    let root = manifest
        .parent()
        .and_then(|p| p.parent())
        .ok_or("missing repository root")?;
    let idl_root = root.join("capabilities/lib");
    let out = PathBuf::from(std::env::var("OUT_DIR")?);
    let selected = out.join("input_contracts");
    std::fs::create_dir_all(&selected)?;
    println!("cargo:rerun-if-changed={}", idl_root.display());
    for (index, relative) in [
        "capabilities/system/soma/get_health.v1.toml",
        "capabilities/system/vitals/get.v1.toml",
        "capabilities/system/scene/get_robot_context.v1.toml",
    ]
    .iter()
    .enumerate()
    {
        let path = root.join(relative);
        println!("cargo:rerun-if-changed={}", path.display());
        std::fs::copy(path, selected.join(format!("{index}.toml")))?;
    }
    let mut resolver = msg_parser::MsgResolver::new(std::slice::from_ref(&idl_root))?;
    let mut skipped = 0;
    resolver.resolve_all_in_index(false, &mut skipped)?;
    resolver.resolve_all_srv(false, &mut skipped)?;
    let services: BTreeSet<_> = contract_gen::collect_referenced_srvs(&selected)?;
    proto_gen::generate(&resolver, &out, Some(&services), false)?;
    contract_gen::generate(&mut resolver, &[selected], &out, false)?;
    let protoc = std::env::var_os("PROTOC")
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(protoc_bin_vendored::protoc_bin_path)?;
    let files: Vec<_> = std::fs::read_dir(&out)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .filter(|path| path.extension().is_some_and(|ext| ext == "proto"))
        .collect();
    let mut config = tonic_prost_build::Config::new();
    config.protoc_executable(protoc);
    // JSON has no non-finite numbers. Serde's default would turn them into
    // null, which an optional SRDL field interprets as a missing observation.
    let responses = services
        .iter()
        .filter_map(|(pkg, name)| resolver.srv_spec(pkg, name))
        .map(|service| &service.response);
    for message in resolver.ordered_specs().into_iter().chain(responses) {
        for field in &message.fields {
            if matches!(&field.type_ref, msg_parser::MsgTypeRef::Primitive(name)
                if matches!(name.as_str(), "float32" | "float64"))
            {
                let function = if field.is_array {
                    "finite_slice"
                } else {
                    "finite_number"
                };
                config.field_attribute(
                    format!(
                        ".{}.{}.{}",
                        proto_gen::proto_package_name(&message.package),
                        message.name,
                        field.name
                    ),
                    format!(r#"#[serde(serialize_with = "super::super::{function}")]"#),
                );
            }
        }
    }
    tonic_prost_build::configure()
        .type_attribute(".", "#[derive(serde::Serialize)]")
        .compile_with_config(config, &files, &[out])?;
    Ok(())
}
