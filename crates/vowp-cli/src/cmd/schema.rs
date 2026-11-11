//! `vowp schema` — exported JSON Schemas, including the v0.2 snapshot.
//!
//! The v0.1 listing and type lookup keep working unchanged. `snapshot` selects the
//! versioned [`vowp_protocol::Snapshot`] schema.

use crate::cli::SchemaArgs;
use crate::output::print_json;
use crate::{CliError, Ctx, IoStreams};

/// Runs `vowp schema`.
pub fn run(ctx: &Ctx, io: &mut dyn IoStreams, args: &SchemaArgs) -> Result<i32, CliError> {
    match args.type_name.as_deref() {
        None => {
            let names: Vec<&str> = vowp_protocol::schema::all_schemas().keys().copied().collect();
            if ctx.json() {
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
