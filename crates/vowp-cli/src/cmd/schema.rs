//! `vowp schema` — exported JSON Schemas, including the v0.2 snapshot.
//!
//! The v0.1 listing and type lookup keep working unchanged. `snapshot` selects the
//! versioned [`vowp_protocol::Snapshot`] schema.

use crate::cli::SchemaArgs;
use crate::output::print_json;
use crate::{CliError, IoStreams};

/// Runs `vowp schema`.
///
/// This handler is context-free: the schema list is compiled into the binary, so it needs
/// no project, `vowp.toml` or initialised home. The CLI calls it before [`crate::Ctx::build`]
/// and passes only the global `--json` flag.
pub fn run(json: bool, io: &mut dyn IoStreams, args: &SchemaArgs) -> Result<i32, CliError> {
    match args.type_name.as_deref() {
        None => {
            let names: Vec<&str> = vowp_protocol::schema::all_schemas().keys().copied().collect();
            if json {
                print_json(io, &names)?;
            } else {
                for name in names {
                    writeln!(io.out(), "{name}")?;
                }
            }
        }
        Some("snapshot") => {
            let schema = vowp_protocol::schema::schema_for_name("Snapshot")
                .ok_or_else(|| CliError::NotFound("schema Snapshot".to_owned()))?;
            print_json(io, &schema)?;
        }
        Some(name) => {
            let schema = vowp_protocol::schema::schema_for_name(name)
                .ok_or_else(|| CliError::NotFound(format!("schema {name}")))?;
            print_json(io, &schema)?;
        }
    }
    Ok(0)
}
