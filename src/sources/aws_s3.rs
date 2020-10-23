use super::util::MultilineConfig;
use crate::{
    config::{DataType, GlobalOptions, SourceConfig, SourceDescription},
    dns::Resolver,
    event::Event,
    line_agg::{self, LineAgg},
    rusoto,
    shutdown::ShutdownSignal,
    Pipeline,
};
use bytes::Bytes;
use codec::BytesDelimitedCodec;
use futures::{
    compat::{Compat, Future01CompatExt},
    future::{FutureExt, TryFutureExt},
    stream::{Stream, StreamExt},
};
use futures01::Sink;
use rusoto_core::{Region, RusotoError};
use rusoto_s3::{GetObjectError, GetObjectRequest, S3Client, S3};
use rusoto_sqs::{
    DeleteMessageError, DeleteMessageRequest, GetQueueUrlError, GetQueueUrlRequest, Message,
    ReceiveMessageError, ReceiveMessageRequest, Sqs, SqsClient,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use snafu::{ResultExt, Snafu};
use std::{convert::TryInto, time::Duration};
use tokio::{select, time};
use tokio_util::codec::FramedRead;

// TODO:
// * Revisit configuration of queue. Should we take the URL instead?
//   * At the least, support setting a differente queue owner
// * Revisit configuration of SQS strategy (intrnal vs. external tagging)
// * Make sure we are handling shutdown well
// * Consider having helper methods stream data and have top-level forward to pipeline
// * Consider / decide on custom endpoint support
//   * How would we handle this for multi-region S3 support?
// * Internal events
//
// Future work:
// * Additional codecs. Just treating like `file` source with newlines for now

#[derive(Copy, Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Compression {
    Auto,
    None,
    Gzip,
    Zstd,
}

impl Default for Compression {
    fn default() -> Self {
        Compression::Auto
    }
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum Strategy {
    Sqs,
}

impl Default for Strategy {
    fn default() -> Self {
        Strategy::Sqs
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct AwsS3Config {
    compression: Compression,

    strategy: Strategy,

    sqs: Option<SqsConfig>,

    assume_role: Option<String>,

    multiline: Option<MultilineConfig>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SqsConfig {
    region: Region,
    queue_name: String,
    #[serde(default = "default_poll_interval_secs")]
    poll_secs: u64,
    #[serde(default = "default_visibility_timeout_secs")]
    visibility_timeout_secs: u64,
    #[serde(default = "default_true")]
    delete_message: bool,
}

const fn default_poll_interval_secs() -> u64 {
    15
}

const fn default_visibility_timeout_secs() -> u64 {
    300
}
const fn default_true() -> bool {
    true
}

inventory::submit! {
    SourceDescription::new::<AwsS3Config>("aws_s3")
}

impl_generate_config_from_default!(AwsS3Config);

#[async_trait::async_trait]
#[typetag::serde(name = "aws_s3")]
impl SourceConfig for AwsS3Config {
    async fn build(
        &self,
        _name: &str,
        _globals: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        let multiline_config: Option<line_agg::Config> = self
            .multiline
            .as_ref()
            .map(|config| config.try_into())
            .map_or(Ok(None), |r| r.map(Some))?;

        match self.strategy {
            Strategy::Sqs => Ok(Box::new(
                self.create_sqs_ingestor(multiline_config)
                    .await?
                    .run(out, shutdown)
                    .boxed()
                    .compat(),
            )),
        }
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "aws_s3"
    }
}

impl AwsS3Config {
    async fn create_sqs_ingestor(
        &self,
        multiline: Option<line_agg::Config>,
    ) -> Result<SqsIngestor, CreateSqsIngestorError> {
        match self.sqs {
            Some(ref sqs) => {
                let resolver = Resolver;
                let client = rusoto::client(resolver).with_context(|| Client {})?;
                let creds: std::sync::Arc<rusoto::AwsCredentialsProvider> =
                    rusoto::AwsCredentialsProvider::new(&sqs.region, self.assume_role.clone())
                        .with_context(|| Credentials {})?
                        .into();
                let sqs_client =
                    SqsClient::new_with(client.clone(), creds.clone(), sqs.region.clone());
                let s3_client =
                    S3Client::new_with(client.clone(), creds.clone(), sqs.region.clone());

                SqsIngestor::new(
                    sqs.region.clone(),
                    sqs_client,
                    s3_client,
                    sqs.clone(),
                    self.compression,
                    multiline,
                )
                .await
                .with_context(|| Initialize {})
            }
            None => Err(CreateSqsIngestorError::ConfigMissing {}),
        }
    }
}

#[derive(Debug, Snafu)]
enum CreateSqsIngestorError {
    #[snafu(display("Unable to initialize: {}", source))]
    Initialize { source: SqsIngestorNewError },
    #[snafu(display("Unable to create AWS client: {}", source))]
    Client { source: crate::Error },
    #[snafu(display("Unable to create AWS credentials provider: {}", source))]
    Credentials { source: crate::Error },
    #[snafu(display("sqs configuration required when strategy=sqs"))]
    ConfigMissing,
}

#[derive(Debug, Snafu)]
enum SqsIngestorNewError {
    #[snafu(display("Unable to fetch queue URL for {}: {}", name, source))]
    FetchQueueUrl {
        source: RusotoError<GetQueueUrlError>,
        name: String,
    },
    #[snafu(display("Got an empty queue URL for {}", name))]
    MissingQueueUrl { name: String },
    #[snafu(display("Invalid visibility timeout {}: {}", timeout, source))]
    InvalidVisibilityTimeout {
        source: std::num::TryFromIntError,
        timeout: u64,
    },
}

#[derive(Debug, Snafu)]
enum ProcessingError {
    #[snafu(display(
        "Could not parse SQS message with id {} as S3 notification: {}",
        message_id,
        source
    ))]
    InvalidSqsMessage {
        source: serde_json::Error,
        message_id: String,
    },
    #[snafu(display("Failed to fetch s3://{}/{}: {}", bucket, key, source))]
    GetObject {
        source: RusotoError<GetObjectError>,
        bucket: String,
        key: String,
    },
    #[snafu(display("Failed to read all of s3://{}/{}: {}", bucket, key, error))]
    ReadObject {
        error: String,
        bucket: String,
        key: String,
    },
    #[snafu(display(
        "Object notification for s3://{}/{} is a bucket in another region: {}",
        bucket,
        key,
        region
    ))]
    WrongRegion {
        region: String,
        bucket: String,
        key: String,
    },
}

struct SqsIngestor {
    region: Region,

    s3_client: S3Client,
    sqs_client: SqsClient,

    multiline: Option<line_agg::Config>,
    compression: Compression,

    queue_url: String,
    poll_interval: Duration,
    visibility_timeout_secs: i64,
    delete_message: bool,
}

impl SqsIngestor {
    async fn new(
        region: Region,
        sqs_client: SqsClient,
        s3_client: S3Client,
        config: SqsConfig,
        compression: Compression,
        multiline: Option<line_agg::Config>,
    ) -> Result<SqsIngestor, SqsIngestorNewError> {
        let queue_url_result = sqs_client
            .get_queue_url(GetQueueUrlRequest {
                queue_name: config.queue_name.clone(),
                ..Default::default()
            })
            .await
            .with_context(|| FetchQueueUrl {
                name: config.queue_name.clone(),
            })?;

        let queue_url = queue_url_result
            .queue_url
            .ok_or(SqsIngestorNewError::MissingQueueUrl {
                name: config.queue_name.clone(),
            })?;

        // This is a bit odd as AWS wants an i64 for this value, but also doesn't want negative
        // values so I used u64 for the config deserialization and validate that there is no
        // overflow here
        let visibility_timeout_secs: i64 =
            config
                .visibility_timeout_secs
                .try_into()
                .context(InvalidVisibilityTimeout {
                    timeout: config.visibility_timeout_secs,
                })?;

        Ok(SqsIngestor {
            region,

            s3_client,
            sqs_client,

            compression,
            multiline,

            queue_url,
            poll_interval: Duration::from_secs(config.poll_secs),
            visibility_timeout_secs,
            delete_message: config.delete_message,
        })
    }

    async fn run(self, out: Pipeline, mut shutdown: ShutdownSignal) -> Result<(), ()> {
        let mut interval = time::interval(self.poll_interval).map(|_| ());

        loop {
            select! {
                Some(()) = interval.next() => (),
                _ = &mut shutdown => break Ok(()),
                else => break Ok(()),
            };

            let messages = self.receive_messages().await.unwrap_or_default(); // TODO emit event for errors

            for message in messages {
                let receipt_handle = match message.receipt_handle {
                    None => {
                        // I don't think this will ever actually happen, but is just an artifact of the
                        // AWS's API prediliction for returning nullable values for all response
                        // attributes
                        warn!(message = "Refusing to process message with no receipt_handle.", ?message.message_id);
                        continue;
                    }
                    Some(ref handle) => handle.to_owned(),
                };

                match self.handle_sqs_message(message, out.clone()).await {
                    Ok(()) => {
                        if self.delete_message {
                            self.delete_message(receipt_handle).await.unwrap(); // TODO emit event
                        }
                    }
                    Err(_err) => {} // TODO emit event
                }
            }
        }
    }

    async fn handle_sqs_message(
        &self,
        message: Message,
        out: Pipeline,
    ) -> Result<(), ProcessingError> {
        let s3_event: S3Event = serde_json::from_str(message.body.unwrap_or_default().as_ref())
            .context(InvalidSqsMessage {
                message_id: message.message_id.unwrap_or("<empty>".to_owned()),
            })?;

        self.handle_s3_event(s3_event, out).await
    }

    async fn handle_s3_event(
        &self,
        s3_event: S3Event,
        out: Pipeline,
    ) -> Result<(), ProcessingError> {
        for record in s3_event.records {
            self.handle_s3_event_record(record, out.clone()).await?
        }
        Ok(())
    }

    async fn handle_s3_event_record(
        &self,
        s3_event: S3EventRecord,
        out: Pipeline,
    ) -> Result<(), ProcessingError> {
        if s3_event.event_name.kind != "ObjectCreated" {
            // TODO emit event
            return Ok(());
        }

        // S3 has to send notifications to a queue in the same region so I don't think this will
        // actually ever be it unless messages are being forwarded from one queue to another
        if self.region.name() != s3_event.aws_region {
            return Err(ProcessingError::WrongRegion {
                bucket: s3_event.s3.bucket.name.clone(),
                key: s3_event.s3.object.key.clone(),
                region: s3_event.aws_region,
            });
        }

        let object = self
            .s3_client
            .get_object(GetObjectRequest {
                bucket: s3_event.s3.bucket.name.clone(),
                key: s3_event.s3.object.key.clone(),
                ..Default::default()
            })
            .await
            .context(GetObject {
                bucket: s3_event.s3.bucket.name.clone(),
                key: s3_event.s3.object.key.clone(),
            })?;

        let metadata = object.metadata;

        match object.body {
            Some(body) => {
                let r = s3_object_decoder(
                    self.compression,
                    &s3_event.s3.object.key,
                    object.content_encoding.as_deref(),
                    object.content_type.as_deref(),
                    body,
                );

                // Record the read error saw to propagate up later so we avoid ack'ing the SQS
                // message
                //
                // String is used as we cannot take clone std::io::Error to take ownership in
                // closure
                //
                // FramedRead likely stops when it gets an i/o error but I found it more clear to
                // show that we `take_while` there hasn't been an error
                //
                // This can result in objects being partially processed before an error, but we
                // prefer duplicate lines over message loss. Future work could include recording
                // the offset of the object that has been read, but this would only be relevant in
                // the case that the same vector instance processes the same message.
                let mut read_error: Option<String> = None;
                let mut lines: Box<dyn Stream<Item = Bytes> + Send + Unpin> = Box::new(
                    FramedRead::new(r, BytesDelimitedCodec::new(b'\n'))
                        .take_while(|r| {
                            futures::future::ready(match r {
                                Ok(_) => true,
                                Err(err) => {
                                    read_error = Some(err.to_string());
                                    false
                                }
                            })
                        })
                        .map(|r| r.unwrap()), // validated by take_while
                );
                if let Some(config) = &self.multiline {
                    lines = Box::new(
                        LineAgg::new(
                            lines.map(|line| ((), line, ())),
                            line_agg::Logic::new(config.clone()),
                        )
                        .map(|(_src, line, _context)| line),
                    );
                }

                let stream = lines.filter_map(|line| {
                    let mut event = Event::from(line);

                    let log = event.as_mut_log();
                    log.insert("bucket", s3_event.s3.bucket.name.clone());
                    log.insert("object", s3_event.s3.object.key.clone());
                    log.insert("region", s3_event.aws_region.clone());

                    if let Some(metadata) = &metadata {
                        for (key, value) in metadata {
                            log.insert(key, value.clone());
                        }
                    }

                    futures::future::ready(Some(Ok(event)))
                });

                out.send_all(Compat::new(Box::pin(stream)))
                    .compat()
                    .await
                    .map_err(|error| {
                        error!(message = "Error sending S3 Logs", %error);
                    })
                    .ok();

                read_error
                    .map(|error| {
                        Err(ProcessingError::ReadObject {
                            error,
                            bucket: s3_event.s3.bucket.name.clone(),
                            key: s3_event.s3.object.key.clone(),
                        })
                    })
                    .unwrap_or(Ok(()))
            }
            None => Ok(()),
        }
    }

    async fn receive_messages(&self) -> Result<Vec<Message>, RusotoError<ReceiveMessageError>> {
        self.sqs_client
            .receive_message(ReceiveMessageRequest {
                queue_url: self.queue_url.clone(),
                max_number_of_messages: Some(10),
                visibility_timeout: Some(self.visibility_timeout_secs),
                ..Default::default()
            })
            .map_ok(|res| res.messages.unwrap_or_default())
            .await
    }

    async fn delete_message(
        &self,
        receipt_handle: String,
    ) -> Result<(), RusotoError<DeleteMessageError>> {
        self.sqs_client
            .delete_message(DeleteMessageRequest {
                queue_url: self.queue_url.clone(),
                receipt_handle,
                ..Default::default()
            })
            .await
    }
}

fn s3_object_decoder(
    compression: Compression,
    key: &str,
    content_encoding: Option<&str>,
    content_type: Option<&str>,
    body: rusoto_s3::StreamingBody,
) -> Box<dyn tokio::io::AsyncRead + Send + Unpin> {
    use async_compression::tokio_02::bufread;

    let r = tokio::io::BufReader::new(body.into_async_read());

    let mut compression = compression;
    if let Auto = compression {
        compression =
            determine_compression(key, content_encoding, content_type).unwrap_or(Compression::None);
    };

    use Compression::*;
    match compression {
        Auto => unreachable!(), // is mapped above
        None => Box::new(r),
        Gzip => Box::new(bufread::GzipDecoder::new(r)),
        Zstd => Box::new(bufread::ZstdDecoder::new(r)),
    }
}

/// try to determine the compression given the:
/// * content-encoding
/// * content-type
/// * key name (for file extension)
///
/// It will use this information in this order
fn determine_compression(
    key: &str,
    content_encoding: Option<&str>,
    content_type: Option<&str>,
) -> Option<Compression> {
    content_encoding
        .and_then(|e| content_encoding_to_compression(e))
        .or_else(|| content_type.and_then(|t| content_type_to_compression(t)))
        .or_else(|| object_key_to_compression(key))
}

fn content_encoding_to_compression(content_encoding: &str) -> Option<Compression> {
    use Compression::*;
    match content_encoding {
        "gzip" => Some(Gzip),
        "zstd" => Some(Zstd),
        _ => Option::None,
    }
}

fn content_type_to_compression(content_type: &str) -> Option<Compression> {
    use Compression::*;
    match content_type {
        "application/gzip" | "application/x-gzip" => Some(Gzip),
        "application/zstd" => Some(Zstd),
        _ => Option::None,
    }
}

fn object_key_to_compression(key: &str) -> Option<Compression> {
    let extension = std::path::Path::new(key)
        .extension()
        .and_then(std::ffi::OsStr::to_str);

    use Compression::*;
    extension.and_then(|extension| match extension {
        "gz" => Some(Gzip),
        "zst" => Some(Zstd),
        _ => Option::None,
    })
}

// https://docs.aws.amazon.com/AmazonS3/latest/dev/notification-content-structure.html
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
struct S3Event {
    records: Vec<S3EventRecord>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3EventRecord {
    event_version: String, // TODO compare >=
    event_source: String,
    aws_region: String,
    event_name: S3EventName,

    s3: S3Message,
}

#[derive(Clone, Debug)]
struct S3EventName {
    kind: String,
    name: String,
}

// https://docs.aws.amazon.com/AmazonS3/latest/dev/NotificationHowTo.html#supported-notification-event-types
//
// we could enums here, but that seems overly brittle as deserialization would  break if they add
// new event types or names
impl<'de> Deserialize<'de> for S3EventName {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        use serde::de::Error;

        let s = String::deserialize(deserializer)?;

        let mut parts = s.splitn(2, ":");

        let kind = parts
            .next()
            .ok_or(D::Error::custom("Missing event type"))?
            .parse::<String>()
            .map_err(D::Error::custom)?;

        let name = parts
            .next()
            .ok_or(D::Error::custom("Missing event name"))?
            .parse::<String>()
            .map_err(D::Error::custom)?;

        Ok(S3EventName { kind, name })
    }
}

impl Serialize for S3EventName {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&format!("{}:{}", self.name, self.kind))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3Message {
    bucket: S3Bucket,
    object: S3Object,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3Bucket {
    name: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct S3Object {
    key: String,
}

mod test {
    #[test]
    fn determine_compression() {
        use super::Compression;

        let cases = vec![
            ("out.log", Some("gzip"), None, Some(Compression::Gzip)),
            (
                "out.log",
                None,
                Some("application/gzip"),
                Some(Compression::Gzip),
            ),
            ("out.log.gz", None, None, Some(Compression::Gzip)),
            ("out.txt", None, None, None),
        ];
        for (key, content_encoding, content_type, expected) in cases {
            assert_eq!(
                super::determine_compression(key, content_encoding, content_type),
                expected
            );
        }
    }
}

#[cfg(feature = "aws-s3-integration-tests")]
#[cfg(test)]
mod integration_tests {
    use super::{AwsS3Config, Compression, SqsConfig, Strategy};
    use crate::{
        config::{GlobalOptions, SourceConfig},
        line_agg,
        shutdown::ShutdownSignal,
        sources::util::MultilineConfig,
        test_util::{collect_n, random_lines},
        Pipeline,
    };
    use futures::compat::Future01CompatExt;
    use pretty_assertions::assert_eq;
    use rusoto_core::Region;
    use rusoto_s3::{PutObjectRequest, S3Client, S3};
    use rusoto_sqs::{Sqs, SqsClient};

    #[tokio::test]
    async fn s3_process_message() {
        let key = uuid::Uuid::new_v4().to_string();
        let logs: Vec<String> = random_lines(100).take(10).collect();

        test_event(&key, None, None, None, logs.join("\n").into_bytes(), logs).await;
    }

    #[tokio::test]
    async fn s3_process_message_gzip() {
        use std::io::Read;

        let key = uuid::Uuid::new_v4().to_string();
        let logs: Vec<String> = random_lines(100).take(10).collect();

        let mut gz = flate2::read::GzEncoder::new(
            std::io::Cursor::new(logs.join("\n").into_bytes()),
            flate2::Compression::fast(),
        );
        let mut buffer = Vec::new();
        gz.read_to_end(&mut buffer).unwrap();

        test_event(&key, Some("gzip"), None, None, buffer, logs).await;
    }

    #[tokio::test]
    async fn s3_process_message_multiline() {
        let key = uuid::Uuid::new_v4().to_string();
        let logs: Vec<String> = vec!["abc", "def", "geh"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();

        test_event(
            &key,
            None,
            None,
            Some(MultilineConfig {
                start_pattern: "abc".to_owned(),
                mode: line_agg::Mode::ContinueThrough,
                condition_pattern: "def".to_owned(),
                timeout_ms: 1000,
            }),
            logs.join("\n").into_bytes(),
            vec!["abc\ndef\ngeh".to_owned()],
        )
        .await;
    }

    async fn config(queue_name: &str, multiline: Option<MultilineConfig>) -> AwsS3Config {
        AwsS3Config {
            strategy: Strategy::Sqs,
            compression: Compression::Auto,
            multiline,
            sqs: Some(SqsConfig {
                queue_name: queue_name.to_string(),
                region: Region::Custom {
                    name: "minio".to_owned(),
                    endpoint: "http://localhost:4566".to_owned(),
                },
                poll_secs: 1,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    // puts an object and asserts that the logs it gets back match
    async fn test_event(
        key: &str,
        content_encoding: Option<&str>,
        content_type: Option<&str>,
        multiline: Option<MultilineConfig>,
        payload: Vec<u8>,
        expected_lines: Vec<String>,
    ) {
        let s3 = s3_client();
        let sqs = sqs_client();

        let queue = create_queue(&sqs).await;
        let bucket = create_bucket(&s3, &queue).await;

        let config = config(&queue, multiline).await;

        s3.put_object(PutObjectRequest {
            bucket: bucket.to_owned(),
            key: key.to_owned(),
            body: Some(rusoto_core::ByteStream::from(payload)),
            content_type: content_type.map(|t| t.to_owned()),
            content_encoding: content_encoding.map(|t| t.to_owned()),
            ..Default::default()
        })
        .await
        .expect("Could not put object");

        let (tx, rx) = Pipeline::new_test();
        tokio::spawn(async move {
            config
                .build(
                    "default",
                    &GlobalOptions::default(),
                    ShutdownSignal::noop(),
                    tx,
                )
                .await
                .unwrap()
                .compat()
                .await
                .unwrap()
        });

        let events = collect_n(rx, expected_lines.len()).await.unwrap();

        assert_eq!(expected_lines.len(), events.len());
        for (i, event) in events.iter().enumerate() {
            let message = expected_lines[i].as_str();

            let log = event.as_log();
            assert_eq!(log["message"], message.into());
            assert_eq!(log["bucket"], bucket.clone().into());
            assert_eq!(log["object"], key.clone().into());
            assert_eq!(log["region"], "us-east-1".into());
        }
    }

    /// creates a new SQS queue
    ///
    /// returns the queue name
    async fn create_queue(client: &SqsClient) -> String {
        use rusoto_sqs::CreateQueueRequest;

        let queue_name = uuid::Uuid::new_v4().to_string();

        client
            .create_queue(CreateQueueRequest {
                queue_name: queue_name.clone(),
                ..Default::default()
            })
            .await
            .expect("Could not create queue");

        queue_name
    }

    /// creates a new bucket with notifications to given SQS queue
    ///
    /// returns the bucket name
    async fn create_bucket(client: &S3Client, queue_name: &str) -> String {
        use rusoto_s3::{
            CreateBucketRequest, NotificationConfiguration,
            PutBucketNotificationConfigurationRequest, QueueConfiguration,
        };

        let bucket_name = uuid::Uuid::new_v4().to_string();

        client
            .create_bucket(CreateBucketRequest {
                bucket: bucket_name.clone(),
                ..Default::default()
            })
            .await
            .expect("Could not create bucket");

        client
            .put_bucket_notification_configuration(PutBucketNotificationConfigurationRequest {
                bucket: bucket_name.clone(),
                notification_configuration: NotificationConfiguration {
                    queue_configurations: Some(vec![QueueConfiguration {
                        events: vec!["s3:ObjectCreated:*".to_string()],
                        queue_arn: format!("arn:aws:sqs:us-east-1:000000000000:{}", queue_name),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .expect("Could not create bucket notification");

        bucket_name
    }

    fn s3_client() -> S3Client {
        let region = Region::Custom {
            name: "minio".to_owned(),
            endpoint: "http://localhost:4566".to_owned(),
        };

        S3Client::new(region)
    }

    fn sqs_client() -> SqsClient {
        let region = Region::Custom {
            name: "minio".to_owned(),
            endpoint: "http://localhost:4566".to_owned(),
        };

        SqsClient::new(region)
    }
}
