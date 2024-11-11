#[macro_use]
extern crate lazy_static;
pub mod auth;
pub mod cli;
pub mod config;
pub mod device_update;
pub mod docker;
pub mod file;
pub mod image;
pub mod ssh;
mod validators;
use anyhow::{Context, Result};
use cli::{
    Command,
    Docker::Inject,
    File::{CopyFromImage, CopyToImage},
    IdentityConfig::{
        SetConfig, SetDeviceCertificate, SetDeviceCertificateNoEst, SetIotLeafSasConfig,
        SetIotedgeGatewayConfig,
    },
    IotHubDeviceUpdate::{self, SetDeviceConfig as IotHubDeviceUpdateSet},
    SshConfig::{SetCertificate, SetConnection},
};
use file::{compression::Compression, functions::FileCopyToParams};
use log::error;
use std::{fs, path::PathBuf};
use tokio::fs::remove_dir_all;
use uuid::Uuid;

use crate::file::compression;

struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            error!("cannot create tokio runtime");
            return;
        };

        rt.block_on(async {
            if let Err(e) = remove_dir_all(self.0.clone()).await {
                error!("cannot remove tmp dir: {e}")
            }
        })
    }
}

fn run_image_command<F>(
    image_file: PathBuf,
    generate_bmap: bool,
    target_compression: Option<Compression>,
    command: F,
) -> Result<()>
where
    F: FnOnce(&PathBuf) -> Result<()>,
{
    if let Ok("true") | Ok("1") = std::env::var("CONTAINERIZED").as_deref() {
        anyhow::ensure!(
            !generate_bmap,
            "run_image_command: generating bmap file is not supported in containerized environments."
        );
    }

    anyhow::ensure!(
        image_file.try_exists().is_ok_and(|exists| exists),
        "run_image_command: image doesn't exist {}",
        image_file.to_str().context("cannot get image file path")?
    );

    let mut dest_image_file = image_file.clone();

    // create /tmp/{uuid}/ and copy image into
    let tmp_dir = PathBuf::from(format!("/tmp/{}", Uuid::new_v4()));
    fs::create_dir_all(tmp_dir.clone()).context(format!(
        "run_image_command: couldn't create destination path {}",
        tmp_dir.to_str().context("cannot get tmp dir name")?
    ))?;

    let _guard = TempDirGuard(tmp_dir.clone());

    let mut tmp_image_file = tmp_dir.join(
        image_file
            .file_name()
            .context("cannot get image file name")?,
    );

    // if applicable decompress image to *.wic
    if let Some(source_compression) = Compression::from_file(&image_file)? {
        std::fs::copy(&image_file, &tmp_image_file)?;
        tmp_image_file = compression::decompress(&tmp_image_file, &source_compression)?;
        dest_image_file.set_extension("");
    } else {
        // copy sparse file (std::fs::copy isn't able)
        libfs::copy_file(&image_file, &tmp_image_file).context(format!(
            "error: libfs::copy_file({:?}, {:?})",
            image_file, tmp_image_file
        ))?;
    }

    // run command
    command(&tmp_image_file)?;

    // create and copy back bmap file if one was created
    if generate_bmap {
        let mut target_bmap = image_file
            .parent()
            .context("cannot get parent dir of image path")?
            .to_path_buf();
        let tmp_bmap = PathBuf::from(format!(
            "{}.bmap",
            tmp_image_file
                .to_str()
                .context("cannot get image file path")?
        ));
        file::functions::generate_bmap_file(
            tmp_image_file
                .to_str()
                .context("cannot get image file path")?,
        )?;
        target_bmap.push(tmp_bmap.file_name().context("cannot get bmap file name")?);
        std::fs::copy(&tmp_bmap, &target_bmap).context(format!(
            "error: std::fs::copy({:?}, {:?})",
            tmp_bmap, target_bmap
        ))?;
    }

    // if applicable compress image
    if let Some(c) = target_compression {
        tmp_image_file = compression::compress(&tmp_image_file, &c)?;
        dest_image_file.set_file_name(
            tmp_image_file
                .file_name()
                .context("cannot get image file name")?,
        );
        std::fs::copy(&tmp_image_file, &dest_image_file).context(format!(
            "error: std::fs::copy({:?}, {:?})",
            tmp_image_file, dest_image_file
        ))?;
    } else {
        // copy sparse file (std::fs::copy isn't able)
        libfs::copy_file(&tmp_image_file, &dest_image_file).context(format!(
            "error: libfs::copy_file({:?}, {:?})",
            tmp_image_file, dest_image_file
        ))?;
    }

    Ok(())
}

pub fn run() -> Result<()> {
    match cli::from_args() {
        Command::Docker(Inject {
            docker_image,
            image,
            partition,
            dest,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img| {
            anyhow::ensure!(
                dest.to_string_lossy().ends_with(".tar.gz"),
                format!(
                    "invalid destination file path \"{}\". Must end in \".tar.gz\".",
                    dest.to_string_lossy(),
                ),
            );

            let arch = image::image_arch(img)?;

            let docker_path = docker::pull_image(&docker_image, arch)?;

            let result = file::copy_to_image(
                &[FileCopyToParams::new(
                    &docker_path,
                    partition.clone(),
                    &dest,
                )],
                img,
            );
            std::fs::remove_file(docker_path)?;

            if result.is_ok() {
                println!(
                    "Stored {} to {}:{}",
                    docker_image,
                    partition,
                    dest.to_string_lossy(),
                );
            }

            result
        })?,
        Command::Identity(SetConfig {
            config,
            image,
            payload,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img| {
            file::set_identity_config(&config, img, payload.as_deref())
        })?,
        Command::Identity(SetDeviceCertificate {
            intermediate_full_chain_cert,
            intermediate_key,
            image,
            device_id,
            days,
            generate_bmap,
            compress_image,
        }) => {
            let intermediate_full_chain_cert_str =
                std::fs::read_to_string(&intermediate_full_chain_cert)
                    .context("couldn't read intermediate fullchain cert")?;
            let intermediate_key_str = std::fs::read_to_string(intermediate_key)
                .context("couldn't read intermediate key")?;
            let crypto = omnect_crypto::Crypto::new(
                intermediate_key_str.as_bytes(),
                intermediate_full_chain_cert_str.as_bytes(),
            )?;
            let (device_cert_pem, device_key_pem) = crypto
                .create_cert_and_key(&device_id, &None, days)
                .context("couldn't create device cert and key")?;

            let device_cert_path = file::get_file_path(&image, "device_cert_path.pem")?;
            let device_key_path = file::get_file_path(&image, "device_key_path.key.pem")?;

            fs::write(&device_cert_path, device_cert_pem)
                .context("set_device_cert: write device_cert_path")?;
            fs::write(&device_key_path, device_key_pem)
                .context("set_device_cert: write device_key_path")?;

            run_image_command(image, generate_bmap, compress_image, |img| {
                file::set_device_cert(
                    Some(&intermediate_full_chain_cert),
                    &device_cert_path,
                    &device_key_path,
                    img,
                )
            })?
        }
        Command::Identity(SetDeviceCertificateNoEst {
            device_cert: device_cert_pem,
            device_key: device_key_pem,
            image,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img| {
            file::set_device_cert(None, &device_cert_pem, &device_key_pem, img)
        })?,
        Command::Identity(SetIotedgeGatewayConfig {
            config,
            image,
            root_ca,
            device_identity,
            device_identity_key,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img: &PathBuf| {
            file::set_iotedge_gateway_config(
                &config,
                img,
                &root_ca,
                &device_identity,
                &device_identity_key,
            )
        })?,
        Command::Identity(SetIotLeafSasConfig {
            config,
            image,
            root_ca,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img: &PathBuf| {
            file::set_iot_leaf_sas_config(&config, img, &root_ca)
        })?,
        Command::Ssh(SetCertificate {
            image,
            root_ca,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img: &PathBuf| {
            file::set_ssh_tunnel_certificate(img, &root_ca)
        })?,
        Command::IotHubDeviceUpdate(IotHubDeviceUpdateSet {
            iot_hub_device_update_config,
            image,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img: &PathBuf| {
            file::set_iot_hub_device_update_config(&iot_hub_device_update_config, img)
        })?,
        Command::IotHubDeviceUpdate(IotHubDeviceUpdate::ImportUpdate {
            import_manifest: import_manifest_path,
            storage_container_name,
            tenant_id,
            client_id,
            client_secret,
            instance_id,
            device_update_endpoint_url,
            blob_storage_account,
            blob_storage_key,
        }) => device_update::import_update(
            &import_manifest_path,
            &storage_container_name,
            &tenant_id,
            &client_id,
            &client_secret,
            &instance_id,
            &device_update_endpoint_url,
            &blob_storage_account,
            &blob_storage_key,
        )?,
        Command::IotHubDeviceUpdate(IotHubDeviceUpdate::RemoveUpdate {
            tenant_id,
            client_id,
            client_secret,
            instance_id,
            device_update_endpoint_url,
            provider,
            distro_name,
            version,
        }) => device_update::remove_update(
            &tenant_id,
            &client_id,
            &client_secret,
            &instance_id,
            &device_update_endpoint_url,
            &provider,
            &distro_name,
            &version,
        )?,
        Command::IotHubDeviceUpdate(IotHubDeviceUpdate::CreateImportManifest {
            image,
            script,
            manufacturer,
            model,
            compatibilityid,
            provider,
            consent_handler,
            swupdate_handler,
            distro_name,
            version,
        }) => device_update::create_import_manifest(
            &image,
            &script,
            &manufacturer,
            &model,
            &compatibilityid,
            &provider,
            &consent_handler,
            &swupdate_handler,
            &distro_name,
            &version,
        )?,
        Command::Ssh(SetConnection {
            device,
            username,
            dir,
            priv_key_path,
            config_path,
            env,
        }) => {
            #[tokio::main]
            async fn create_ssh_tunnel(
                device: &str,
                username: &str,
                dir: Option<PathBuf>,
                priv_key_path: Option<PathBuf>,
                config_path: Option<PathBuf>,
                env_config: config::BackendConfig,
            ) -> Result<()> {
                let access_token = crate::auth::authorize(env_config.auth)
                    .await
                    .context("create ssh tunnel")?;

                let config = ssh::Config::new(env_config.backend, dir, priv_key_path, config_path)?;

                ssh::ssh_create_tunnel(device, username, config, access_token).await
            }

            let env_conf: config::BackendConfig = if let Some(env_path) = env {
                let config_file = std::fs::read_to_string(env_path)?;

                toml::from_str(&config_file)?
            } else {
                config::BackendConfig {
                    backend: url::Url::parse("https://cp.omnect.conplement.cloud")?,
                    auth: config::AUTH_INFO_PROD.clone(),
                }
            };

            create_ssh_tunnel(
                &device,
                &username,
                dir,
                priv_key_path,
                config_path,
                env_conf,
            )?;
        }
        Command::File(CopyToImage {
            file_copy_params,
            image,
            generate_bmap,
            compress_image,
        }) => run_image_command(image, generate_bmap, compress_image, |img: &PathBuf| {
            file::copy_to_image(&file_copy_params, img)
        })?,
        Command::File(CopyFromImage {
            file_copy_params,
            image,
        }) => run_image_command(image, false, None, |img: &PathBuf| {
            file::copy_from_image(&file_copy_params, img)
        })?,
    }

    Ok(())
}
