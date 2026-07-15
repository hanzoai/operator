//! CRD YAML bundle generator.
//!
//! Emits one multi-document YAML stream carrying every CRD this operator
//! manages, at the requested API group. The canonical Kind set + group-rewrite
//! live in `operator::install::crd_bundle` (the same code `operator install`
//! applies), so this binary is a thin formatter over it — one home for the
//! bundle, no duplicated Kind list.
//!
//! Usage:
//!
//! ```text
//! generate-crd-yaml [--api-group <group>]
//! ```
//!
//! `--api-group` (or `OPERATOR_API_GROUP`) defaults to `hanzo.ai`. Other
//! universes: `lux.cloud`, `zoo.cloud`, `osage.cloud`.

use operator::api_group::ApiGroup;
use operator::install::crd_bundle;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Tiny hand-rolled flag parse — this binary takes exactly one optional flag
    // and pulling `clap` into a generator is unwarranted weight.
    let mut api_group: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--api-group" => {
                api_group = Some(args.next().ok_or("--api-group requires a value")?);
            }
            other if other.starts_with("--api-group=") => {
                api_group = Some(other["--api-group=".len()..].to_string());
            }
            "-h" | "--help" => {
                eprintln!("usage: generate-crd-yaml [--api-group <group>]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    let group = ApiGroup::resolve(api_group.as_deref()).group;
    let mut out = String::new();
    for crd in crd_bundle(&group) {
        out.push_str("---\n");
        out.push_str(&serde_yaml::to_string(&crd)?);
    }
    print!("{out}");
    Ok(())
}
