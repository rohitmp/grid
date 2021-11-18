// Copyright 2018-2021 Cargill Incorporated
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::pin::Pin;
use std::str::FromStr;

use futures::prelude::*;
use protobuf::Message;
use sawtooth_sdk::messages::batch::Batch;

use super::{
    BackendClient, BackendClientError, BatchStatus, BatchStatusLink, BatchStatuses,
    InvalidTransaction, SubmitBatches,
};

macro_rules! try_fut {
    ($try_expr:expr) => {
        match $try_expr {
            Ok(res) => res,
            Err(err) => return futures::future::err(err).boxed(),
        }
    };
}

#[derive(Clone)]
pub struct SplinterBackendClient {
    node_url: String,
    authorization: String,
}

impl SplinterBackendClient {
    /// Constructs a new splinter BackendClient instance, using the given url for the node's REST
    /// API.
    pub fn new(node_url: String, authorization: String) -> Self {
        Self {
            node_url,
            authorization,
        }
    }
}

impl BackendClient for SplinterBackendClient {
    fn submit_batches(
        &self,
        msg: SubmitBatches,
    ) -> Pin<Box<dyn Future<Output = Result<BatchStatusLink, BackendClientError>> + Send>> {
        let service_arg = try_fut!(msg.service_id.ok_or_else(|| {
            BackendClientError::BadRequestError("A service id must be provided".into())
        }));

        let service_info = try_fut!(SplinterService::from_str(&service_arg));

        let url = format!(
            "{}/scabbard/{}/{}/batches",
            self.node_url, service_info.circuit_id, service_info.service_id
        );

        let batch_list_bytes = try_fut!(msg.batch_list.write_to_bytes().map_err(|err| {
            BackendClientError::BadRequestError(format!("Malformed batch list: {}", err))
        }));

        let batch_query = msg
            .batch_list
            .get_batches()
            .iter()
            .map(Batch::get_header_signature)
            .collect::<Vec<_>>()
            .join(",");
        let mut response_url = msg.response_url;
        response_url.set_query(Some(&format!("id={}", batch_query)));
        let link = response_url.to_string();

        reqwest::Client::new()
            .post(&url)
            .header("GridProtocolVersion", "1")
            .header("Content-Type", "octet-stream")
            .header("Authorization", &self.authorization.to_string())
            .body(batch_list_bytes)
            .send()
            .then(|res| {
                future::ready(match res {
                    Ok(_) => Ok(BatchStatusLink { link }),
                    Err(err) => Err(BackendClientError::InternalError(format!(
                        "Unable to submit batch: {}",
                        err
                    ))),
                })
            })
            .boxed()
    }

    fn batch_status(
        &self,
        msg: BatchStatuses,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<BatchStatus>, BackendClientError>> + Send>> {
        let service_arg = try_fut!(msg.service_id.ok_or_else(|| {
            BackendClientError::BadRequestError("A service id must be provided".into())
        }));

        let service_info = try_fut!(SplinterService::from_str(&service_arg));

        // {base_url}/scabbard/{circuit_id}/{service_id}/batch_statuses?[wait={time}&]ids={batch_ids}
        let mut url = self.node_url.clone();
        url.push_str("/scabbard/");
        url.push_str(&service_info.circuit_id);
        url.push('/');
        url.push_str(&service_info.service_id);
        url.push_str("/batch_statuses?");

        if let Some(wait_time) = msg.wait {
            url.push_str("wait=");
            url.push_str(&wait_time.to_string());
            url.push('&');
        }

        url.push_str("ids=");
        url.push_str(&msg.batch_ids.join(","));

        reqwest::Client::new()
            .get(&url)
            .header("GridProtocolVersion", "1")
            .header("Authorization", &self.authorization.to_string())
            .send()
            .then(|res| match res {
                Ok(res) => res.json().boxed(),
                Err(err) => future::err(err).boxed(),
            })
            .map(|result| {
                result
                    .map(|stats: Vec<SplinterBatchStatus>| {
                        stats.into_iter().map(|status| status.into()).collect()
                    })
                    .map_err(|err| {
                        BackendClientError::InternalError(format!(
                            "Unable to retrieve batch statuses: {}",
                            err
                        ))
                    })
            })
            .boxed()
    }

    fn clone_box(&self) -> Box<dyn BackendClient> {
        Box::new(self.clone())
    }
}

#[derive(Deserialize, Debug)]
struct SplinterBatchStatus {
    id: String,
    status: Status,
}

#[derive(Deserialize, Debug)]
struct Status {
    #[serde(rename(deserialize = "statusType"))]
    status_type: String,
    message: Vec<ErrorMessage>,
}

#[derive(Deserialize, Debug)]
struct ErrorMessage {
    transaction_id: String,
    error_message: Option<String>,
    error_data: Option<Vec<u8>>,
}

impl From<SplinterBatchStatus> for BatchStatus {
    fn from(batch_status: SplinterBatchStatus) -> Self {
        Self {
            id: batch_status.id,
            status: batch_status.status.status_type,
            invalid_transactions: batch_status
                .status
                .message
                .into_iter()
                .filter(|message| message.error_message.is_some() && message.error_data.is_some())
                .map(|message| InvalidTransaction {
                    id: message.transaction_id,
                    message: message.error_message.unwrap(),
                    extended_data: base64::encode(&message.error_data.unwrap()),
                })
                .collect(),
        }
    }
}

struct SplinterService {
    circuit_id: String,
    service_id: String,
}

impl FromStr for SplinterService {
    type Err = BackendClientError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split("::");
        let circuit_id: String = parts
            .next()
            .ok_or_else(|| {
                BackendClientError::BadRequestError("Empty service_id parameter provided".into())
            })?
            .into();
        let service_id: String = parts
            .next()
            .ok_or_else(|| {
                BackendClientError::BadRequestError(
                    "Must provide a fully-qualified service_id: <circuit_id>::<service_id>".into(),
                )
            })?
            .into();

        Ok(Self {
            circuit_id,
            service_id,
        })
    }
}
