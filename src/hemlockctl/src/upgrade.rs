//! In-band OS image upgrade: read the image header locally for a quick
//! sanity check and a useful prompt, then have mgmtd (which runs as
//! root) perform the install through the InstallImage RPC — the same
//! path the web console uses.

use anyhow::{bail, Context, Result};
use hemlock_common::ipc::IpcEndpoint;
use hemlock_common::proto::v1 as pb;

pub async fn run(endpoint: IpcEndpoint, image: &str, force: bool, reboot: bool) -> Result<()> {
    let path = std::path::Path::new(image);
    let header = hemlock_common::image::read_header(path).map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("image: hemlock {} for {}", header.version, header.platform);

    // mgmtd resolves the path in its own working directory; send it
    // absolute so relative operator paths work.
    let absolute = std::fs::canonicalize(path).with_context(|| format!("resolving {image}"))?;
    let channel = endpoint.connect().await.context("connecting to mgmtd")?;
    let mut client = pb::mgmt_client::MgmtClient::new(channel);

    println!("installing — this can take a few minutes ...");
    let response = match client
        .install_image(pb::InstallImageRequest {
            path: absolute.display().to_string(),
            force,
            reboot,
        })
        .await
    {
        Ok(response) => response.into_inner(),
        Err(status) => bail!("{}", status.message()),
    };
    if reboot {
        println!("hemlock {} installed — rebooting now", response.version);
    } else {
        println!("hemlock {} installed — reboot to run it", response.version);
    }
    Ok(())
}
