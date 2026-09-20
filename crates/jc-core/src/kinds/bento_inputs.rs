//! The inputs the pinned runner ships (PL-50): names, the fields Bento marks secret, which
//! inputs end on their own. Generated from `bento list --format json-full` of
//! ghcr.io/warpstreamlabs/bento:v1.21.1 by joinedcontext-portal/ui/scripts/bento-inputs.mjs;
//! change the pin and rerun the script, never edit by hand. The script appends its own
//! `ALSO_SECRET` list to what the runner marks: fields the runner's documentation does not flag
//! and that are credentials all the same, per input, after the tree walk (T-2239).

/// The runner release this catalog was generated from.
pub const RUNNER_VERSION: &str = "v1.21.1";

/// Every input name the runner accepts as `spec.type` beside the typed four.
pub const INPUTS: &[&str] = &[
    "amqp_0_9",
    "amqp_1",
    "aws_kinesis",
    "aws_s3",
    "aws_sqs",
    "azure_blob_storage",
    "azure_cosmosdb",
    "azure_queue_storage",
    "azure_service_bus_queue",
    "azure_table_storage",
    "batched",
    "beanstalkd",
    "broker",
    "cassandra",
    "cockroachdb_changefeed",
    "csv",
    "cypher",
    "discord",
    "dynamic",
    "etcd",
    "file",
    "file_tail",
    "fsevent",
    "gcp_bigquery_select",
    "gcp_cloud_storage",
    "gcp_pubsub",
    "gcp_spanner_cdc",
    "generate",
    "grpc_client",
    "hdfs",
    "http_client",
    "http_server",
    "inproc",
    "kafka",
    "kafka_franz",
    "kubernetes_watch",
    "mongodb",
    "mqtt",
    "nanomsg",
    "nats",
    "nats_jetstream",
    "nats_kv",
    "nats_object_store",
    "nats_stream",
    "nsq",
    "parquet",
    "pulsar",
    "read_until",
    "redis_list",
    "redis_pubsub",
    "redis_scan",
    "redis_streams",
    "resource",
    "s2",
    "sequence",
    "sftp",
    "socket",
    "socket_server",
    "sql_raw",
    "sql_select",
    "stdin",
    "subprocess",
    "twitter_search",
    "websocket",
    "zmq4n",
];

/// Per input, the field paths the runner documents as secrets; `[]` is every element of an
/// array of objects. A manifest holds a `${VAR}` interpolation there, never a value (MF-35).
pub const SECRET_FIELDS: &[(&str, &[&str])] = &[
    (
        "amqp_0_9",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    (
        "amqp_1",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "sasl.password",
        ],
    ),
    (
        "aws_kinesis",
        &["credentials.secret", "credentials.id", "credentials.token"],
    ),
    (
        "aws_s3",
        &["credentials.secret", "credentials.id", "credentials.token"],
    ),
    (
        "aws_sqs",
        &["credentials.secret", "credentials.id", "credentials.token"],
    ),
    ("azure_blob_storage", &[]),
    ("azure_cosmosdb", &["account_key", "connection_string"]),
    ("azure_queue_storage", &[]),
    ("azure_service_bus_queue", &[]),
    ("azure_table_storage", &[]),
    ("batched", &[]),
    ("beanstalkd", &[]),
    ("broker", &[]),
    (
        "cassandra",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "password_authenticator.password",
        ],
    ),
    (
        "cockroachdb_changefeed",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    ("csv", &[]),
    (
        "cypher",
        &[
            "basic_auth.password",
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    ("discord", &["bot_token"]),
    ("dynamic", &[]),
    (
        "etcd",
        &[
            "auth.password",
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    ("file", &[]),
    ("file_tail", &[]),
    ("fsevent", &[]),
    ("gcp_bigquery_select", &[]),
    ("gcp_cloud_storage", &[]),
    ("gcp_pubsub", &[]),
    ("gcp_spanner_cdc", &[]),
    ("generate", &[]),
    (
        "grpc_client",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "oauth2.client_secret",
        ],
    ),
    ("hdfs", &[]),
    (
        "http_client",
        &[
            "oauth.consumer_secret",
            "oauth.access_token_secret",
            "oauth2.client_secret",
            "basic_auth.password",
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "negotiate.user.password",
            "oauth.access_token",
            "digest_auth.password",
        ],
    ),
    ("http_server", &[]),
    ("inproc", &[]),
    (
        "kafka",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "sasl.password",
            "sasl.oauth2.client_secret",
            "sasl.aws.credentials.secret",
            "sasl.access_token",
            "sasl.aws.credentials.id",
            "sasl.aws.credentials.token",
        ],
    ),
    (
        "kafka_franz",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "sasl[].password",
            "sasl[].oauth2.client_secret",
            "sasl[].aws.credentials.secret",
        ],
    ),
    ("kubernetes_watch", &["kubeconfig_yaml", "token"]),
    ("mongodb", &["password"]),
    (
        "mqtt",
        &[
            "password",
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    ("nanomsg", &[]),
    (
        "nats",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "auth.user_jwt",
            "auth.user_nkey_seed",
        ],
    ),
    (
        "nats_jetstream",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "auth.user_jwt",
            "auth.user_nkey_seed",
        ],
    ),
    (
        "nats_kv",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "auth.user_jwt",
            "auth.user_nkey_seed",
        ],
    ),
    (
        "nats_object_store",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "auth.user_jwt",
            "auth.user_nkey_seed",
        ],
    ),
    (
        "nats_stream",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "auth.user_jwt",
            "auth.user_nkey_seed",
        ],
    ),
    (
        "nsq",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "auth_secret",
        ],
    ),
    ("parquet", &[]),
    ("pulsar", &["auth.token.token"]),
    ("read_until", &[]),
    (
        "redis_list",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    (
        "redis_pubsub",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    (
        "redis_scan",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    (
        "redis_streams",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
        ],
    ),
    ("resource", &[]),
    ("s2", &["access_token"]),
    ("sequence", &[]),
    (
        "sftp",
        &["credentials.password", "credentials.private_key_pass"],
    ),
    ("socket", &[]),
    ("socket_server", &[]),
    (
        "sql_raw",
        &["credentials.secret", "credentials.id", "credentials.token"],
    ),
    (
        "sql_select",
        &["credentials.secret", "credentials.id", "credentials.token"],
    ),
    ("stdin", &[]),
    ("subprocess", &[]),
    ("twitter_search", &["api_key"]),
    (
        "websocket",
        &[
            "tls.root_cas",
            "tls.client_certs[].key",
            "tls.client_certs[].password",
            "oauth.consumer_secret",
            "oauth.access_token_secret",
            "basic_auth.password",
            "oauth.access_token",
        ],
    ),
    ("zmq4n", &[]),
];

/// Inputs that end on their own once they have read what there is (a scheduled pipeline may
/// run one, PL-04); every other input is a broker or socket and stays resident.
pub const TERMINATING: &[&str] = &[
    "aws_s3",
    "azure_blob_storage",
    "azure_table_storage",
    "cassandra",
    "csv",
    "file",
    "gcp_bigquery_select",
    "gcp_cloud_storage",
    "generate",
    "hdfs",
    "parquet",
    "read_until",
    "redis_scan",
    "sequence",
    "sftp",
    "sql_raw",
    "sql_select",
    "stdin",
    "subprocess",
];

/// Inputs that read the runner's files volume; their paths stay under `/data/`.
pub const FILE_READERS: &[&str] = &["csv", "file", "file_tail", "parquet"];
