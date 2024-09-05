//
// Copyright (c) 2022 ZettaScale Technology
//
// This program and the accompanying materials are made available under the
// terms of the Eclipse Public License 2.0 which is available at
// http://www.eclipse.org/legal/epl-2.0, or the Apache License, Version 2.0
// which is available at https://www.apache.org/licenses/LICENSE-2.0.
//
// SPDX-License-Identifier: EPL-2.0 OR Apache-2.0
//
// Contributors:
//   ZettaScale Zenoh Team, <zenoh@zettascale.tech>
//

use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    future::Future,
    str::FromStr,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as b64_std_engine, Engine};
use influxdb::{
    Client, ReadQuery as InfluxRQuery, Timestamp as InfluxTimestamp, WriteQuery as InfluxWQuery,
};
use serde::Deserialize;
use tracing::{debug, error, warn};
use uuid::Uuid;
use zenoh::{
    bytes::Encoding,
    internal::{bail, buffers::ZBuf, zerror, Value},
    key_expr::{keyexpr, KeyExpr, OwnedKeyExpr},
    query::{Parameters, TimeBound, TimeExpr, TimeRange},
    time::Timestamp,
    try_init_log_from_env, Error, Result as ZResult,
};
use zenoh_backend_traits::{
    config::{PrivacyGetResult, PrivacyTransparentGet, StorageConfig, VolumeConfig},
    StorageInsertionResult, *,
};
use zenoh_plugin_trait::{plugin_long_version, plugin_version, Plugin};

const WORKER_THREAD_NUM: usize = 2;
const MAX_BLOCK_THREAD_NUM: usize = 50;
lazy_static::lazy_static! {
    // The global runtime is used in the dynamic plugins, which we can't get the current runtime
    static ref TOKIO_RUNTIME: tokio::runtime::Runtime = tokio::runtime::Builder::new_multi_thread()
               .worker_threads(WORKER_THREAD_NUM)
               .max_blocking_threads(MAX_BLOCK_THREAD_NUM)
               .enable_all()
               .build()
               .expect("Unable to create runtime");
}

#[inline(always)]
fn blockon_runtime<F: Future>(task: F) -> F::Output {
    // Check whether able to get the current runtime
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => {
            // Able to get the current runtime (standalone binary), spawn on the current runtime
            tokio::task::block_in_place(|| rt.block_on(task))
        }
        Err(_) => {
            // Unable to get the current runtime (dynamic plugins), spawn on the global runtime
            tokio::task::block_in_place(|| TOKIO_RUNTIME.block_on(task))
        }
    }
}

// Properties used by the Backend
pub const PROP_BACKEND_URL: &str = "url";
pub const PROP_BACKEND_USERNAME: &str = "username";
pub const PROP_BACKEND_PASSWORD: &str = "password";

// Properties used by the Storage
pub const PROP_STORAGE_DB: &str = "db";
pub const PROP_STORAGE_CREATE_DB: &str = "create_db";
pub const PROP_STORAGE_ON_CLOSURE: &str = "on_closure";
pub const PROP_STORAGE_USERNAME: &str = PROP_BACKEND_USERNAME;
pub const PROP_STORAGE_PASSWORD: &str = PROP_BACKEND_PASSWORD;
pub const PROP_STORAGE_PUT_BATCH_SIZE: &str = "put_batch_size";
pub const PROP_STORAGE_PUT_BATCH_TIMEOUT_MS: &str = "put_batch_timeout_ms";

// Special key for None (when the prefix being stripped exactly matches the key)
pub const NONE_KEY: &str = "@@none_key@@";

// delay after deletion to drop a measurement
const DROP_MEASUREMENT_TIMEOUT_MS: u64 = 5000;

// default batch timeout
const DEFAULT_BATCH_TIMEOUT_MS: u64 = 1000;

lazy_static::lazy_static!(
    static ref INFLUX_REGEX_ALL: String = key_exprs_to_influx_regex(&["**".try_into().unwrap()]);
);

fn get_private_conf<'a>(
    config: &'a serde_json::Map<String, serde_json::Value>,
    credit: &str,
) -> ZResult<Option<&'a String>> {
    match config.get_private(credit) {
        PrivacyGetResult::NotFound => Ok(None),
        PrivacyGetResult::Private(serde_json::Value::String(v)) => Ok(Some(v)),
        PrivacyGetResult::Public(serde_json::Value::String(v)) => {
            tracing::warn!(
                r#"Value "{}" is given for `{}` publicly (i.e. is visible by anyone who can fetch the router configuration). You may want to replace `{}: "{}"` with `private: {{{}: "{}"}}`"#,
                v,
                credit,
                credit,
                v,
                credit,
                v
            );
            Ok(Some(v))
        }
        PrivacyGetResult::Both {
            public: serde_json::Value::String(public),
            private: serde_json::Value::String(private),
        } => {
            tracing::warn!(
                r#"Value "{}" is given for `{}` publicly, but a private value also exists. The private value will be used, but the public value, which is {} the same as the private one, will still be visible in configurations."#,
                public,
                credit,
                if public == private { "" } else { "not " }
            );
            Ok(Some(private))
        }
        _ => {
            bail!("Optional property `{}` must be a string", credit)
        }
    }
}

pub struct InfluxDbBackend {}

#[cfg(feature = "dynamic_plugin")]
zenoh_plugin_trait::declare_plugin!(InfluxDbBackend);

impl Plugin for InfluxDbBackend {
    type StartArgs = VolumeConfig;
    type Instance = VolumeInstance;

    const DEFAULT_NAME: &'static str = "influxdb_backend";
    const PLUGIN_VERSION: &'static str = plugin_version!();
    const PLUGIN_LONG_VERSION: &'static str = plugin_long_version!();

    fn start(_name: &str, config: &Self::StartArgs) -> ZResult<Self::Instance> {
        try_init_log_from_env();

        debug!("InfluxDB backend {}", Self::PLUGIN_VERSION);

        let mut config = config.clone();
        config
            .rest
            .insert("version".into(), Self::PLUGIN_VERSION.into());

        let url = match config.rest.get(PROP_BACKEND_URL) {
            Some(serde_json::Value::String(url)) => url.clone(),
            _ => {
                bail!(
                    "Mandatory property `{}` for InfluxDb Backend must be a string",
                    PROP_BACKEND_URL
                )
            }
        };

        // The InfluxDB client used for administration purposes (show/create/drop databases)
        let mut admin_client = Client::new(url, "");

        // Note: remove username/password from properties to not re-expose them in admin_status
        let credentials = match (
            get_private_conf(&config.rest, PROP_BACKEND_USERNAME)?,
            get_private_conf(&config.rest, PROP_BACKEND_PASSWORD)?,
        ) {
            (Some(username), Some(password)) => {
                admin_client = admin_client.with_auth(username, password);
                Some((username.clone(), password.clone()))
            }
            (None, None) => None,
            _ => {
                bail!(
                    "Optional properties `{}` and `{}` must coexist",
                    PROP_BACKEND_USERNAME,
                    PROP_BACKEND_PASSWORD
                )
            }
        };

        // Check connectivity to InfluxDB, trying to list databases
        match blockon_runtime(async { show_databases(&admin_client).await }) {
            Ok(dbs) => {
                // trick: if "_internal" db is not shown, it means the credentials are not for an admin
                if !dbs.iter().any(|e| e == "_internal") {
                    warn!("The InfluxDB credentials are not for an admin user; the volume won't be able to create or drop any database")
                }
            }
            Err(e) => bail!("Failed to create InfluxDb Volume : {}", e),
        }

        Ok(Box::new(InfluxDbVolume {
            admin_status: config,
            admin_client,
            credentials,
        }))
    }
}

pub struct InfluxDbVolume {
    admin_status: VolumeConfig,
    admin_client: Client,
    credentials: Option<(String, String)>,
}

#[async_trait]
impl Volume for InfluxDbVolume {
    fn get_admin_status(&self) -> serde_json::Value {
        self.admin_status.to_json_value()
    }

    fn get_capability(&self) -> Capability {
        Capability {
            persistence: Persistence::Durable,
            history: History::All,
            read_cost: 1,
        }
    }

    async fn create_storage(&self, mut config: StorageConfig) -> ZResult<Box<dyn Storage>> {
        let volume_cfg = match config.volume_cfg.as_object() {
            Some(v) => v,
            None => bail!("InfluxDB backed storages need some volume-specific configuration"),
        };

        // batching
        let put_batch_size: Option<usize> = match volume_cfg.get(PROP_STORAGE_PUT_BATCH_SIZE) {
            Some(v) => v.as_u64().map(|v| v as usize),
            None => None,
        };
        let put_batch_timeout_ms = match volume_cfg.get(PROP_STORAGE_PUT_BATCH_TIMEOUT_MS) {
            Some(v) => v.as_u64().unwrap_or(DEFAULT_BATCH_TIMEOUT_MS),
            None => DEFAULT_BATCH_TIMEOUT_MS,
        };

        let put_batch_timeout = Duration::from_millis(put_batch_timeout_ms);

        let on_closure = match volume_cfg.get(PROP_STORAGE_ON_CLOSURE) {
            Some(serde_json::Value::String(x)) if x == "drop_series" => OnClosure::DropSeries,
            Some(serde_json::Value::String(x)) if x == "drop_db" => OnClosure::DropDb,
            Some(serde_json::Value::String(x)) if x == "do_nothing" => OnClosure::DoNothing,
            None => OnClosure::DoNothing,
            Some(_) => {
                bail!(
                    r#"`{}` property of storage `{}` must be one of "do_nothing" (default), "drop_db" and "drop_series""#,
                    PROP_STORAGE_ON_CLOSURE,
                    &config.name
                )
            }
        };
        let (db, createdb) = match volume_cfg.get(PROP_STORAGE_DB) {
            Some(serde_json::Value::String(s)) => (
                s.clone(),
                match volume_cfg.get(PROP_STORAGE_CREATE_DB) {
                    None | Some(serde_json::Value::Bool(false)) => false,
                    Some(serde_json::Value::Bool(true)) => true,
                    Some(_) => todo!(),
                },
            ),
            None => (generate_db_name(), true),
            Some(v) => bail!("Invalid value for ${PROP_STORAGE_DB} config property: ${v}"),
        };

        // The Influx client on database used to write/query on this storage
        // (using the same URL than backend's admin_client, but with storage credentials)
        let mut client = Client::new(self.admin_client.database_url(), &db);

        // Use credentials if specified in storage's volume config
        let storage_username = match (
            get_private_conf(volume_cfg, PROP_STORAGE_USERNAME)?,
            get_private_conf(volume_cfg, PROP_STORAGE_PASSWORD)?,
        ) {
            (Some(username), Some(password)) => {
                client = client.with_auth(username, password);
                Some(username.clone())
            }
            (None, None) => None,
            _ => {
                bail!(
                    "Optional properties `{}` and `{}` must coexist",
                    PROP_STORAGE_USERNAME,
                    PROP_STORAGE_PASSWORD
                )
            }
        };

        // Check if the database exists (using storages credentials)
        if !is_db_existing(&client, &db).await? {
            if createdb {
                // create db using backend's credentials
                create_db(&self.admin_client, &db, storage_username).await?;
            } else {
                bail!("Database '{}' doesn't exist in InfluxDb", db)
            }
        }

        // re-insert the actual name of database (in case it has been generated)
        config
            .volume_cfg
            .as_object_mut()
            .unwrap()
            .entry(PROP_STORAGE_DB)
            .or_insert(db.clone().into());

        // The Influx client on database with backend's credentials (admin), to drop measurements and database
        let mut admin_client = Client::new(self.admin_client.database_url(), db);
        if let Some((username, password)) = &self.credentials {
            admin_client = admin_client.with_auth(username, password);
        }

        // Collect PUT requests and send in batches for efficiency?
        let put_batch_tx = if let Some(put_batch_size) = put_batch_size {
            debug!(
                "[{}] PUT queries will be sent in batches of {} or after {:#?}",
                config.name, put_batch_size, put_batch_timeout
            );

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Put>();

            let client_clone = client.clone();
            let name_clone = config.name.clone();
            let batch_future = async move {
                let mut put_batch: Vec<InfluxWQuery> = Vec::new();
                let mut measurement_counts: HashMap<OwnedKeyExpr, u64> = HashMap::new();

                let mut batch_start_time = Instant::now();
                loop {
                    if put_batch.is_empty() {
                        // waiting for first batch item...
                        match rx.recv().await {
                            Some(Put { query, measurement }) => {
                                // begin batch...
                                put_batch.push(query);
                                measurement_counts
                                    .entry(measurement)
                                    .and_modify(|counter| *counter += 1)
                                    .or_insert(1);
                                batch_start_time = Instant::now();
                            }
                            None => {
                                debug!("[{}] batch put channel closed, exiting task", name_clone,);
                                break;
                            }
                        }
                    } else {
                        // ...and wait for more items
                        match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                            Ok(r) => match r {
                                Some(Put { query, measurement }) => {
                                    // add to batch
                                    put_batch.push(query);
                                    measurement_counts
                                        .entry(measurement)
                                        .and_modify(|counter| *counter += 1)
                                        .or_insert(1);
                                }
                                None => {
                                    debug!(
                                        "[{}] batch put channel closed, exiting task",
                                        name_clone
                                    );
                                    break;
                                }
                            },
                            Err(_) => {
                                // timeout
                            }
                        };

                        let batch_full = put_batch.len() >= put_batch_size;
                        let batch_timed_out = batch_start_time.elapsed() > put_batch_timeout;

                        if batch_full || batch_timed_out {
                            let n = put_batch.len();

                            let counts: Vec<String> = measurement_counts
                                .drain()
                                .map(|(k, v)| format!("{k} x {v}"))
                                .collect();
                            debug!(
                                "[{}] PUT batch of {} - {}",
                                name_clone,
                                n,
                                counts.join(", ")
                            );
                            let result = client_clone.query(&put_batch).await;
                            put_batch.clear();

                            if let Err(e) = result {
                                debug!(
                                    "[{}] Failed to put Value for batch of {} in InfluxDb storage : {}", name_clone,
                                    n, e
                                )
                            }
                        }
                    }
                }
            };

            match tokio::runtime::Handle::try_current() {
                Ok(handle) => handle.spawn(batch_future),
                Err(_) => TOKIO_RUNTIME.spawn(batch_future),
            };

            Some(tx)
        } else {
            None
        };

        Ok(Box::new(InfluxDbStorage {
            config,
            admin_client,
            client,
            on_closure,
            put_batch_tx,
        }))
    }
}

enum OnClosure {
    DropDb,
    DropSeries,
    DoNothing,
}

impl TryFrom<&Parameters<'_>> for OnClosure {
    type Error = Error;
    fn try_from(p: &Parameters) -> ZResult<OnClosure> {
        match p.get(PROP_STORAGE_ON_CLOSURE) {
            Some(s) => {
                if s == "drop_db" {
                    Ok(OnClosure::DropDb)
                } else if s == "drop_series" {
                    Ok(OnClosure::DropSeries)
                } else {
                    bail!("Unsupported value for 'on_closure' property: {}", s)
                }
            }
            None => Ok(OnClosure::DoNothing),
        }
    }
}

struct Put {
    measurement: OwnedKeyExpr,
    query: InfluxWQuery,
}

struct InfluxDbStorage {
    config: StorageConfig,
    admin_client: Client,
    client: Client,
    on_closure: OnClosure,
    put_batch_tx: Option<tokio::sync::mpsc::UnboundedSender<Put>>,
}

impl InfluxDbStorage {
    async fn get_deletion_timestamp(&self, measurement: &str) -> ZResult<Option<Timestamp>> {
        #[derive(Deserialize, Debug, PartialEq)]
        struct QueryResult {
            timestamp: String,
        }

        let query = InfluxRQuery::new(format!(
            r#"SELECT "timestamp" FROM "{measurement}" WHERE kind='DEL' ORDER BY time DESC LIMIT 1"#
        ));
        match self.client.json_query(query).await {
            Ok(mut result) => match result.deserialize_next::<QueryResult>() {
                Ok(qr) => {
                    if !qr.series.is_empty() && !qr.series[0].values.is_empty() {
                        let ts = qr.series[0].values[0]
                            .timestamp
                            .parse::<Timestamp>()
                            .map_err(|err| {
                                zerror!(
                                "Failed to parse the latest timestamp for deletion of measurement {} : {}",
                                measurement, err.cause)
                            })?;
                        Ok(Some(ts))
                    } else {
                        Ok(None)
                    }
                }
                Err(err) => bail!(
                    "Failed to get latest timestamp for deletion of measurement {} : {}",
                    measurement,
                    err
                ),
            },
            Err(err) => bail!(
                "Failed to get latest timestamp for deletion of measurement {} : {}",
                measurement,
                err
            ),
        }
    }

    async fn schedule_measurement_drop(&self, measurement: &str) {
        let m_string = measurement.to_string();
        let cloned_client = self.client.clone();

        // Wait till timeout expires and execute drop,
        // When this plugin executes as a dynamically loaded plugin,
        // the zenohd tokio-runtime is unavailable, therefore the plugin's runtime must be invoked
        let async_drop = async {
            tokio::time::sleep(Duration::from_millis(DROP_MEASUREMENT_TIMEOUT_MS)).await;
            drop_measurement(m_string, cloned_client).await;
        };

        match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.spawn(async_drop),
            Err(_) => TOKIO_RUNTIME.spawn(async_drop),
        };
    }

    fn keyexpr_from_serie(&self, serie_name: &str) -> ZResult<Option<OwnedKeyExpr>> {
        if serie_name.eq(NONE_KEY) {
            Ok(None)
        } else {
            match OwnedKeyExpr::from_str(serie_name) {
                Ok(key) => Ok(Some(key)),
                Err(e) => Err(format!("{}", e).into()),
            }
        }
    }
}

#[async_trait]
impl Storage for InfluxDbStorage {
    fn get_admin_status(&self) -> serde_json::Value {
        // TODO: possibly add more properties in returned Value for more information about this storage
        self.config.to_json_value()
    }

    async fn put(
        &mut self,
        key: Option<OwnedKeyExpr>,
        value: Value,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let measurement = key.unwrap_or_else(|| OwnedKeyExpr::from_str(NONE_KEY).unwrap());

        // Note: assume that uhlc timestamp was generated by a clock using UNIX_EPOCH (that's the case by default)
        let influx_time = timestamp.get_time().to_duration().as_nanos();

        // get timestamp of deletion of this measurement, if any
        if let Some(del_time) = self.get_deletion_timestamp(measurement.as_str()).await? {
            // ignore sample if oldest than the deletion
            if timestamp < del_time {
                debug!(
                    "Received a value for {:?} with timestamp older than its deletion; ignore it",
                    measurement
                );
                return Ok(StorageInsertionResult::Outdated);
            }
        }

        // encode the value as a string to be stored in InfluxDB, converting to base64 if the buffer is not a UTF-8 string
        let (base64, strvalue) = match value.payload().deserialize::<String>() {
            Ok(s) => (false, s),
            Err(err) => (true, b64_std_engine.encode(err.to_string())),
        };

        // Note: tags are stored as strings in InfluxDB, while fileds are typed.
        // For simpler/faster deserialization, we store encoding, timestamp and base64 as fields.
        // while the kind is stored as a tag to be indexed by InfluxDB and have faster queries on it.
        let encoding_string_rep = value.encoding().to_string(); // add_field only supports Strings and not Vec<u8>
        let encoding: &Encoding = value.encoding();

        let query = InfluxWQuery::new(
            InfluxTimestamp::Nanoseconds(influx_time),
            measurement.clone(),
        )
        .add_tag("kind", "PUT")
        .add_field("timestamp", timestamp.to_string())
        .add_field("encoding_prefix", encoding.id())
        .add_field("encoding_suffix", encoding_string_rep) // TODO: Rename To Encoding and only keep String rep
        .add_field("base64", base64)
        .add_field("value", strvalue);

        match &self.put_batch_tx {
            None => {
                // not batched - send query now
                debug!("Put {:?} with Influx query: {:?}", measurement, query);
                if let Err(e) = self.client.query(&query).await {
                    bail!(
                        "Failed to put Value for {:?} in InfluxDb storage : {}",
                        measurement,
                        e
                    )
                } else {
                    Ok(StorageInsertionResult::Inserted)
                }
            }
            Some(sender) => {
                let put = Put { query, measurement };
                if let Err(e) = sender.send(put) {
                    bail!("Failed to send to batch queue for InfluxDb storage : {}", e)
                } else {
                    // assume success
                    // TODO - add pending status
                    Ok(StorageInsertionResult::Inserted)
                }
            }
        }
    }

    async fn delete(
        &mut self,
        key: Option<OwnedKeyExpr>,
        timestamp: Timestamp,
    ) -> ZResult<StorageInsertionResult> {
        let measurement = key.unwrap_or_else(|| OwnedKeyExpr::from_str(NONE_KEY).unwrap());

        // Note: assume that uhlc timestamp was generated by a clock using UNIX_EPOCH (that's the case by default)
        let influx_time = timestamp.get_time().to_duration().as_nanos();

        // delete all points from the measurement that are older than this DELETE message
        // (in case more recent PUT have been recevived un-ordered)
        let query = InfluxRQuery::new(format!(
            r#"DELETE FROM "{}" WHERE time < {}"#,
            measurement, influx_time
        ));
        debug!("Delete {:?} with Influx query: {:?}", measurement, query);
        if let Err(e) = self.client.query(&query).await {
            bail!(
                "Failed to delete points for measurement '{}' from InfluxDb storage : {}",
                measurement,
                e
            )
        }
        // store a point (with timestamp) with "delete" tag, thus we don't re-introduce an older point later
        let query = InfluxWQuery::new(
            InfluxTimestamp::Nanoseconds(influx_time),
            measurement.clone(),
        )
        .add_tag("kind", "DEL")
        .add_field("timestamp", timestamp.to_string())
        .add_field("encoding_prefix", 0_u8)
        .add_field("encoding_suffix", "")
        .add_field("base64", false)
        .add_field("value", "");
        debug!(
            "Mark measurement {} as deleted at time {}",
            measurement, influx_time
        );
        if let Err(e) = self.client.query(&query).await {
            bail!(
                "Failed to mark measurement {:?} as deleted : {}",
                measurement,
                e
            )
        }
        // schedule the drop of measurement later in the future, if it's empty
        self.schedule_measurement_drop(measurement.as_str()).await;
        Ok(StorageInsertionResult::Deleted)
    }

    async fn get(
        &mut self,
        key: Option<OwnedKeyExpr>,
        parameters: &str,
    ) -> ZResult<Vec<StoredData>> {
        let measurement = match key {
            Some(k) => k,
            None => OwnedKeyExpr::from_str(NONE_KEY).unwrap(),
        };
        // convert the key expression into an Influx regex
        let regex = key_exprs_to_influx_regex(&[&KeyExpr::from(measurement)]);

        // construct the Influx query clauses from the parameters
        let clauses = clauses_from_parameters(parameters)?;

        // the Influx query
        let influx_query_str = format!("SELECT * FROM {regex} {clauses}");
        let influx_query = InfluxRQuery::new(&influx_query_str);

        // the expected JSon type resulting from the query
        #[derive(Deserialize, Debug)]
        struct ZenohPoint {
            #[allow(dead_code)]
            // NOTE: "kind" is present within InfluxDB and used in query clauses, but not read in Rust...
            kind: String,
            timestamp: String,
            encoding_prefix: u8,
            encoding_suffix: String,
            base64: bool,
            value: String,
        }

        let mut result = Vec::new();
        match self.client.json_query(influx_query).await {
            Ok(mut query_result) => {
                while !query_result.results.is_empty() {
                    match query_result.deserialize_next::<ZenohPoint>() {
                        Ok(retn) => {
                            // for each serie
                            for serie in retn.series {
                                // get the key expression from the serie name
                                let ke = match self.keyexpr_from_serie(&serie.name) {
                                    Ok(k) => k,
                                    Err(e) => {
                                        error!(
                                            "Error replying with serie '{}' : {}",
                                            serie.name, e
                                        );
                                        continue;
                                    }
                                };
                                debug!("Replying {} values for {:?}", serie.values.len(), ke);
                                // for each point
                                for zpoint in serie.values {
                                    // get the encoding

                                    let encoding = if zpoint.encoding_suffix.is_empty() {
                                        Encoding::new(zpoint.encoding_prefix.into(), None)
                                    } else {
                                        Encoding::from(zpoint.encoding_suffix)
                                    };
                                    // get the payload
                                    let payload = if zpoint.base64 {
                                        match b64_std_engine.decode(zpoint.value) {
                                            Ok(v) => ZBuf::from(v),
                                            Err(e) => {
                                                warn!(
                                                    r#"Failed to decode zenoh base64 Value from Influx point {} with timestamp="{}": {}"#,
                                                    serie.name, zpoint.timestamp, e
                                                );
                                                continue;
                                            }
                                        }
                                    } else {
                                        ZBuf::from(zpoint.value.into_bytes())
                                    };
                                    // get the timestamp
                                    let timestamp = match Timestamp::from_str(&zpoint.timestamp) {
                                        Ok(t) => t,
                                        Err(e) => {
                                            warn!(
                                                r#"Failed to decode zenoh Timestamp from Influx point {} with timestamp="{}": {:?}"#,
                                                serie.name, zpoint.timestamp, e
                                            );
                                            continue;
                                        }
                                    };
                                    let value = Value::new(payload, encoding);
                                    result.push(StoredData { value, timestamp });
                                }
                            }
                        }
                        Err(e) => {
                            bail!(
                                "Failed to parse result of InfluxDB query '{}': {}",
                                influx_query_str,
                                e
                            )
                        }
                    }
                }
            }
            Err(e) => bail!(
                "Failed to query InfluxDb with '{}' : {}",
                influx_query_str,
                e
            ),
        }
        Ok(result)
    }

    async fn get_all_entries(&self) -> ZResult<Vec<(Option<OwnedKeyExpr>, Timestamp)>> {
        let mut result = Vec::new();

        // the Influx query: 1 entry == 1 measurement => get only 1 point per measurement (the more recent timestamp)
        let influx_query_str = format!(
            "SELECT * FROM {} ORDER BY time DESC LIMIT 1",
            *INFLUX_REGEX_ALL
        );
        let influx_query = InfluxRQuery::new(&influx_query_str);

        // the expected JSon type resulting from the query
        #[derive(Deserialize, Debug)]
        struct ZenohPoint {
            #[allow(dead_code)]
            // NOTE: "kind" is present within InfluxDB and used in query clauses, but not read in Rust...
            kind: String,
            timestamp: String,
        }
        debug!("Get all entries with Influx query: {}", influx_query_str);
        match self.client.json_query(influx_query).await {
            Ok(mut query_result) => {
                while !query_result.results.is_empty() {
                    match query_result.deserialize_next::<ZenohPoint>() {
                        Ok(retn) => {
                            // for each serie
                            for serie in retn.series {
                                // get the key expression from the serie name
                                match self.keyexpr_from_serie(&serie.name) {
                                    Ok(ke) => {
                                        debug!(
                                            "Replying {} values for {:?}",
                                            serie.values.len(),
                                            ke
                                        );
                                        // for each point in the serie
                                        for zpoint in serie.values {
                                            // get the timestamp (ignore the point if failing)
                                            match Timestamp::from_str(&zpoint.timestamp) {
                                                Ok(timestamp) => {
                                                    result.push((ke.clone(), timestamp))
                                                }
                                                Err(e) => warn!(
                                                    r#"Failed to decode zenoh Timestamp from Influx point {} with timestamp="{}": {:?}"#,
                                                    serie.name, zpoint.timestamp, e
                                                ),
                                            };
                                        }
                                    }
                                    Err(e) => {
                                        error!("Error replying with serie '{}' : {}", serie.name, e)
                                    }
                                };
                            }
                        }
                        Err(e) => {
                            bail!(
                                "Failed to parse result of InfluxDB query '{}': {}",
                                influx_query_str,
                                e
                            )
                        }
                    }
                }
                Ok(result)
            }
            Err(e) => bail!(
                "Failed to query InfluxDb with '{}' : {}",
                influx_query_str,
                e
            ),
        }
    }
}

impl Drop for InfluxDbStorage {
    fn drop(&mut self) {
        debug!("Closing InfluxDB storage");
        match self.on_closure {
            OnClosure::DropDb => {
                blockon_runtime(async move {
                    let db = self.admin_client.database_name();
                    debug!("Close InfluxDB storage, dropping database {}", db);
                    let query = InfluxRQuery::new(format!(r#"DROP DATABASE "{db}""#));
                    if let Err(e) = self.admin_client.query(&query).await {
                        error!("Failed to drop InfluxDb database '{}' : {}", db, e)
                    }
                });
            }
            OnClosure::DropSeries => {
                blockon_runtime(async move {
                    let db = self.client.database_name();
                    debug!(
                        "Close InfluxDB storage, dropping all series from database {}",
                        db
                    );
                    let query = InfluxRQuery::new("DROP SERIES FROM /.*/");
                    if let Err(e) = self.client.query(&query).await {
                        error!(
                            "Failed to drop all series from InfluxDb database '{}' : {}",
                            db, e
                        )
                    }
                });
            }
            OnClosure::DoNothing => {
                debug!(
                    "Close InfluxDB storage, keeping database {} as it is",
                    self.client.database_name()
                );
            }
        }
    }
}

async fn drop_measurement(measurement: String, client: Client) {
    #[derive(Deserialize, Debug, PartialEq)]
    struct QueryResult {
        kind: String,
    }

    // check if there is at least 1 point without "DEL" kind in the measurement
    let query = InfluxRQuery::new(format!(
        r#"SELECT "kind" FROM "{}" WHERE kind!='DEL' LIMIT 1"#,
        measurement
    ));
    match client.json_query(query).await {
        Ok(mut result) => {
            match result.deserialize_next::<QueryResult>() {
                Ok(qr) => {
                    if !qr.series.is_empty() {
                        debug!("Measurement {} contains new values inserted after deletion; don't drop it", measurement);
                        return;
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to check if measurement '{}' is empty (can't drop it) : {}",
                        measurement, e
                    );
                }
            }
        }
        Err(e) => {
            warn!(
                "Failed to check if measurement '{}' is empty (can't drop it) : {}",
                measurement, e
            );
            return;
        }
    }

    // drop the measurement
    let query = InfluxRQuery::new(format!(r#"DROP MEASUREMENT "{}""#, measurement));
    debug!(
        "Drop measurement {} after timeout with Influx query: {:?}",
        measurement, query
    );
    if let Err(e) = client.query(&query).await {
        warn!(
            "Failed to drop measurement '{}' from InfluxDb storage : {}",
            measurement, e
        );
    }
}

fn generate_db_name() -> String {
    format!("zenoh_db_{}", Uuid::new_v4().simple())
}

async fn show_databases(client: &Client) -> ZResult<Vec<String>> {
    #[derive(Deserialize)]
    struct Database {
        name: String,
    }
    let query = InfluxRQuery::new("SHOW DATABASES");
    debug!("List databases with Influx query: {:?}", query);
    match client.json_query(query).await {
        Ok(mut result) => match result.deserialize_next::<Database>() {
            Ok(dbs) => {
                let mut result: Vec<String> = Vec::new();
                for serie in dbs.series {
                    for db in serie.values {
                        result.push(db.name);
                    }
                }
                Ok(result)
            }
            Err(e) => bail!(
                "Failed to parse list of existing InfluxDb databases : {}",
                e
            ),
        },
        Err(e) => bail!("Failed to list existing InfluxDb databases : {}", e),
    }
}

async fn is_db_existing(client: &Client, db_name: &str) -> ZResult<bool> {
    let dbs = show_databases(client).await?;
    Ok(dbs.iter().any(|e| e == db_name))
}

async fn create_db(
    client: &Client,
    db_name: &str,
    storage_username: Option<String>,
) -> ZResult<()> {
    let query = InfluxRQuery::new(format!(r#"CREATE DATABASE "{db_name}""#));
    debug!("Create Influx database: {}", db_name);
    if let Err(e) = client.query(&query).await {
        bail!(
            "Failed to create new InfluxDb database '{}' : {}",
            db_name,
            e
        )
    }
    debug!("after await: {}", db_name);
    // is a username is specified for storage access, grant him access to the database
    if let Some(username) = storage_username {
        let query = InfluxRQuery::new(format!(r#"GRANT ALL ON "{db_name}" TO "{username}""#));
        debug!(
            "Grant access to {} on Influx database: {}",
            username, db_name
        );
        if let Err(e) = client.query(&query).await {
            bail!(
                "Failed grant access to {} on Influx database '{}' : {}",
                username,
                db_name,
                e
            )
        }
    }
    Ok(())
}

// Returns an InfluxDB regex (see https://docs.influxdata.com/influxdb/v1.8/query_language/explore-data/#regular-expressions)
// corresponding to the list of path expressions. I.e.:
// Replace "**" with ".*", "*" with "[^\/]*"  and "/" with "\/".
// Concat each with "|", and surround the result with '/^' and '$/'.
fn key_exprs_to_influx_regex(path_exprs: &[&keyexpr]) -> String {
    let mut result = String::with_capacity(2 * path_exprs[0].len());
    result.push_str("/^");
    for (i, path_expr) in path_exprs.iter().enumerate() {
        if i != 0 {
            result.push('|');
        }
        let mut chars = path_expr.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '*' => {
                    if let Some(c2) = chars.peek() {
                        if c2 == &'*' {
                            result.push_str(".*");
                            chars.next();
                        } else {
                            result.push_str(".*")
                        }
                    }
                }
                '/' => result.push_str(r"\/"),
                _ => result.push(c),
            }
        }
    }
    result.push_str("$/");
    result
}

fn clauses_from_parameters(p: &str) -> ZResult<String> {
    let time_range = TimeRange::from_str(p);
    let mut result = String::with_capacity(256);
    result.push_str("WHERE kind!='DEL'");
    match time_range {
        Ok(TimeRange(start, stop)) => {
            match start {
                TimeBound::Inclusive(t) => {
                    result.push_str(" AND time >= ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Exclusive(t) => {
                    result.push_str(" AND time > ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Unbounded => {}
            }
            match stop {
                TimeBound::Inclusive(t) => {
                    result.push_str(" AND time <= ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Exclusive(t) => {
                    result.push_str(" AND time < ");
                    write_timeexpr(&mut result, t);
                }
                TimeBound::Unbounded => {}
            }
        }
        Err(err) => {
            warn!("Error In TimeRange parse from String {}", err);
            //No time selection, return only latest values
            result.push_str(" ORDER BY time DESC LIMIT 1");
        }
    }
    Ok(result)
}

fn write_timeexpr(s: &mut String, t: TimeExpr) {
    use std::fmt::Write;

    use humantime::format_rfc3339;
    match t {
        TimeExpr::Fixed(t) => write!(s, "'{}'", format_rfc3339(t)),
        TimeExpr::Now { offset_secs } => write!(s, "now(){offset_secs:+}s"),
    }
    .unwrap()
}
