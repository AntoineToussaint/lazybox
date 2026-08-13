//! `lazybox device …` — manage revocable per-device credentials.
//!
//! Operates directly on the box's on-disk device registry
//! (`~/.lazybox/v2/identity/`), the same registry the running daemon
//! reads to resolve a device-token bearer to its principal. Minting on
//! the box and revoking a lost device therefore take effect without a
//! daemon restart.

use anyhow::Context;
use lazybox_identity::{BoxIdentity, DeviceRecord, DeviceRegistry};

const USAGE: &str = "usage:\n  \
    lazybox device box [--format base64]\n  \
                                        show this box's pairing identity\n  \
    lazybox device mint --name <name>   mint a credential for a new device\n  \
    lazybox device list                 list paired devices\n  \
    lazybox device revoke <id>          revoke one device\n  \
    lazybox device token <id>           reprint a device's pairing token";

pub async fn device_subcommand(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("box") => box_identity(&args[1..]),
        Some("mint") => mint(&open_registry(), &args[1..]),
        Some("list") => list(&open_registry()),
        Some("revoke") => revoke(&open_registry(), args.get(1).map(String::as_str)),
        Some("token") => token(&open_registry(), args.get(1).map(String::as_str)),
        other => {
            anyhow::bail!(
                "unknown `lazybox device` verb {:?}\n{USAGE}",
                other.unwrap_or("<none>")
            );
        }
    }
}

fn open_registry() -> DeviceRegistry {
    let dir = lazybox_core::paths::identity_dir();
    let keystore = lazybox_identity::default_keystore(&dir);
    DeviceRegistry::open(dir, keystore)
}

fn box_identity(args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let format = crate::take_value(&mut args, "--format");
    let identity = BoxIdentity::load_or_generate(lazybox_core::paths::identity_dir())
        .context("load or generate box identity")?;
    match format.as_deref() {
        None => {
            println!("box id:     {}", identity.box_id());
            println!("public key: {}", identity.public_key_hex());
        }
        Some("base64") => println!("{}", identity.public_key_base64()),
        Some(format) => anyhow::bail!("unknown box identity format {format:?}; expected `base64`"),
    }
    Ok(())
}

fn mint(registry: &DeviceRegistry, args: &[String]) -> anyhow::Result<()> {
    let mut args = args.to_vec();
    let name = crate::take_value(&mut args, "--name")
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty())
        .context("device mint needs a non-empty --name")?;

    let minted = registry.mint(&name).context("mint device credential")?;
    println!("Minted device credential for {name:?}.");
    println!("  id:        {}", minted.record.id);
    println!("  principal: {}", minted.record.principal_id);
    println!();
    println!("Pairing token (shown once — deliver it to the device):");
    println!("  {}", minted.token);
    Ok(())
}

fn list(registry: &DeviceRegistry) -> anyhow::Result<()> {
    let devices = registry.list().context("read device registry")?;
    if devices.is_empty() {
        println!("No paired devices. Mint one with `lazybox device mint --name <name>`.");
        return Ok(());
    }
    for device in &devices {
        println!("{}", format_row(device));
    }
    Ok(())
}

fn format_row(device: &DeviceRecord) -> String {
    let status = match device.revoked_at {
        Some(at) => format!("revoked {}", at.format("%Y-%m-%d")),
        None => "active".to_string(),
    };
    format!(
        "{id}  {name:<20}  {status:<18}  created {created}",
        id = device.id,
        name = device.name,
        status = status,
        created = device.created_at.format("%Y-%m-%d"),
    )
}

fn revoke(registry: &DeviceRegistry, id: Option<&str>) -> anyhow::Result<()> {
    let id = id.context("device revoke needs a device id (see `lazybox device list`)")?;
    if registry.revoke(id).context("revoke device")? {
        println!("Revoked device {id}. It can no longer authenticate.");
    } else {
        println!("No active device with id {id}. Nothing revoked.");
    }
    Ok(())
}

fn token(registry: &DeviceRegistry, id: Option<&str>) -> anyhow::Result<()> {
    let id = id.context("device token needs a device id (see `lazybox device list`)")?;
    match registry.token(id).context("read device token")? {
        Some(token) => {
            println!("{token}");
            Ok(())
        }
        None => anyhow::bail!("no stored token for device {id} (unknown or revoked)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_identity::MemoryKeystore;

    #[test]
    fn format_row_reflects_active_and_revoked() {
        let dir = tempfile::tempdir().unwrap();
        let registry = DeviceRegistry::open(dir.path(), Box::new(MemoryKeystore::new()));
        let phone = registry.mint("iPhone").unwrap();
        registry.mint("Laptop").unwrap();
        registry.revoke(&phone.record.id).unwrap();

        let rows = registry.list().unwrap();
        let phone_row = rows.iter().find(|r| r.name == "iPhone").unwrap();
        let laptop_row = rows.iter().find(|r| r.name == "Laptop").unwrap();
        assert!(format_row(phone_row).contains("revoked"));
        assert!(format_row(laptop_row).contains("active"));
    }
}
