use azure_storage::shared_access_signature::service_sas::BlobSasPermissions;
use azure_storage::{ConnectionString, Error};
use azure_storage_blobs::prelude::BlobServiceClient;
use std::fmt;
use time::{Duration, OffsetDateTime};

const SIGNED_URL_TTL_SECONDS: i64 = 300;

#[derive(Copy, Clone, Debug)]
pub enum DcsBlobVersion {
    V1,
    V2,
}

impl DcsBlobVersion {
    fn container_name(self) -> &'static str {
        match self {
            DcsBlobVersion::V1 => "dcs-v1",
            DcsBlobVersion::V2 => "dcs-v2",
        }
    }
}

pub struct DcsBlobUrlGenerator {
    blob_service_client: BlobServiceClient,
}

#[derive(Debug)]
pub enum DcsBlobUrlGeneratorError {
    ParseConnectionString(Error),
    MissingAccountName,
    ParseStorageCredentials(Error),
    GenerateSasToken(Error),
    GenerateSignedBlobUrl(Error),
}

impl fmt::Display for DcsBlobUrlGeneratorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DcsBlobUrlGeneratorError::ParseConnectionString(error) => {
                write!(f, "failed to parse Azure connection string: {error}")
            }
            DcsBlobUrlGeneratorError::MissingAccountName => {
                write!(f, "missing account name in Azure connection string")
            }
            DcsBlobUrlGeneratorError::ParseStorageCredentials(error) => {
                write!(f, "failed to parse storage credentials: {error}")
            }
            DcsBlobUrlGeneratorError::GenerateSasToken(error) => {
                write!(f, "failed to generate blob SAS token: {error}")
            }
            DcsBlobUrlGeneratorError::GenerateSignedBlobUrl(error) => {
                write!(f, "failed to generate signed blob URL: {error}")
            }
        }
    }
}

impl std::error::Error for DcsBlobUrlGeneratorError {}

impl DcsBlobUrlGenerator {
    pub fn from_connection_string(
        connection_string: &str,
    ) -> Result<Self, DcsBlobUrlGeneratorError> {
        let parsed_connection_string = ConnectionString::new(connection_string)
            .map_err(DcsBlobUrlGeneratorError::ParseConnectionString)?;

        let account_name = parsed_connection_string
            .account_name
            .ok_or(DcsBlobUrlGeneratorError::MissingAccountName)?;

        let storage_credentials = parsed_connection_string
            .storage_credentials()
            .map_err(DcsBlobUrlGeneratorError::ParseStorageCredentials)?;

        Ok(Self {
            blob_service_client: BlobServiceClient::new(account_name, storage_credentials),
        })
    }

    pub fn build_download_url(
        &self,
        version: DcsBlobVersion,
        company_id: &str,
        sdk_key: &str,
    ) -> Result<String, DcsBlobUrlGeneratorError> {
        let blob_name = format!("{company_id}/{sdk_key}");
        let blob_client = self
            .blob_service_client
            .container_client(version.container_name())
            .blob_client(blob_name);

        let expiry = OffsetDateTime::now_utc() + Duration::seconds(SIGNED_URL_TTL_SECONDS);

        let sas = blob_client
            .shared_access_signature(
                BlobSasPermissions {
                    read: true,
                    ..Default::default()
                },
                expiry,
            )
            .map_err(DcsBlobUrlGeneratorError::GenerateSasToken)?;

        blob_client
            .generate_signed_blob_url(&sas)
            .map(|url| url.to_string())
            .map_err(DcsBlobUrlGeneratorError::GenerateSignedBlobUrl)
    }
}

#[cfg(test)]
mod tests {
    use super::{DcsBlobUrlGenerator, DcsBlobVersion};

    #[test]
    fn generates_signed_url_with_expected_path() {
        let generator = DcsBlobUrlGenerator::from_connection_string(
            "DefaultEndpointsProtocol=https;AccountName=idliststorage;AccountKey=ZmFrZS1rZXk=;EndpointSuffix=core.windows.net",
        )
        .expect("connection string should parse");

        let url = generator
            .build_download_url(DcsBlobVersion::V1, "50aWbk2p4R76rNX9lN5VUw", "secret-test")
            .expect("signed URL should generate");

        assert!(url.starts_with(
            "https://idliststorage.blob.core.windows.net/dcs-v1/50aWbk2p4R76rNX9lN5VUw/secret-test"
        ));
        assert!(url.contains("sig="));
    }
}
