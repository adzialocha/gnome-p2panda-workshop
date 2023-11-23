use std::time::{SystemTime, UNIX_EPOCH};

use adw::prelude::*;
use anyhow::{anyhow, Result};
use aquadoggo::{Configuration, LockFile, Node};
use gql_client::Client as GraphQLClient;
use p2panda_rs::identity::KeyPair;
use p2panda_rs::operation::plain::PlainOperation;
use p2panda_rs::operation::OperationBuilder;
use p2panda_rs::schema::SchemaId;
use serde::Deserialize;
use tokio::runtime::Builder;
use tokio::sync::broadcast;

use crate::p2panda::{Client as P2PandaClient, Collection, Document, Meta};
use crate::workbench;

const NODE_ENDPOINT: &str = "http://localhost:2020/graphql";

const BOOKMARKS_SCHEMA_ID: &str =
    "bookmarks_0020017dbdd0193158a00a62a9a8e382d1daa7120de5f3243c8b380f9d0f74e7c524";

type SearchQuery = String;
type Url = String;
type Description = String;

#[derive(Deserialize, Clone, Debug)]
struct BookmarksCollection {
    pub bookmarks: Collection<Bookmark>,
}

#[derive(Deserialize, Clone, Debug)]
#[allow(unused)]
struct Bookmark {
    pub url: String,
    pub description: String,
    pub timestamp: u64,
}

struct Client {
    client: P2PandaClient,
    gql_client: GraphQLClient,
    key_pair: KeyPair,
}

impl Client {
    pub fn new() -> Self {
        let client = P2PandaClient::new(NODE_ENDPOINT);
        let gql_client = GraphQLClient::new(NODE_ENDPOINT);
        let key_pair = KeyPair::new();

        Self {
            client,
            gql_client,
            key_pair,
        }
    }

    pub async fn all_bookmarks(
        &self,
        query: Option<SearchQuery>,
    ) -> Result<Vec<Document<Bookmark>>> {
        let filter = match query {
            Some(query) => {
                format!(
                    r#"
                    filter: {{
                        description: {{ contains: "{query}" }}
                    }}
                    "#
                )
            }
            None => "".to_string(),
        };

        let query = format!(
            r#"
            {{
                bookmarks: all_{BOOKMARKS_SCHEMA_ID}(
                  orderBy: "timestamp",
                  orderDirection: DESC,
                  {filter}
                ) {{
                    documents {{
                        meta {{
                            documentId
                            viewId
                            owner
                        }}
                        fields {{
                            url
                            description
                            timestamp
                        }}
                    }}
                }}
            }}
            "#
        );

        let response = self
            .gql_client
            .query_unwrap::<BookmarksCollection>(&query)
            .await
            .map_err(|err| anyhow!("GraphQL request failed: {err}"))?;

        Ok(response.bookmarks.documents)
    }

    pub async fn add_bookmark(&self, url: &str, description: &str) -> Result<Document<Bookmark>> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("System time invalid, operation system time configured before UNIX epoch")
            .as_secs();

        let schema_id = SchemaId::new(BOOKMARKS_SCHEMA_ID).unwrap();

        let operation = OperationBuilder::new(&schema_id)
            .fields(&[
                ("url", url.into()),
                ("description", description.into()),
                ("timestamp", (timestamp as i64).into()),
            ])
            .build()?;

        let entry_hash = self
            .client
            .sign_and_send(&self.key_pair, &PlainOperation::from(&operation))
            .await?;

        let document = Document {
            meta: Meta {
                document_id: entry_hash.clone().into(),
                view_id: entry_hash.into(),
                owner: self.key_pair.public_key(),
            },
            fields: Bookmark {
                url: url.to_owned(),
                description: description.to_owned(),
                timestamp: timestamp.to_owned(),
            },
        };

        Ok(document)
    }
}

#[derive(Clone, Debug)]
enum Message {
    GetAllBookmarksRequest(Option<SearchQuery>),
    GetAllBookmarksResponse(Vec<Document<Bookmark>>),
    AddBookmarkRequest(Url, Description),
    AddBookmarkResponse(Document<Bookmark>),
}

pub fn main() {
    env_logger::Builder::new()
        .filter(Some("aquadoggo"), log::LevelFilter::Info)
        .init();

    let (sender, _) = broadcast::channel::<Message>(16);

    {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();

        let key_pair = KeyPair::new();
        let config = Configuration::default();
        let node = rt.block_on(Node::start(key_pair, config));

        let data = include_str!("schema/schema.lock");
        let lock_file: LockFile = toml::from_str(&data).expect("error parsing schema.lock file");
        rt.block_on(node.migrate(lock_file))
            .expect("Schema migration failed");

        std::thread::spawn(move || {
            rt.block_on(node.on_exit());
        });
    }

    {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        let sender = sender.clone();
        let mut receiver = sender.subscribe();

        let client = Client::new();

        std::thread::spawn(move || {
            rt.block_on(async move {
                while let Ok(message) = receiver.recv().await {
                    match message {
                        Message::GetAllBookmarksRequest(query) => {
                            let bookmarks = client.all_bookmarks(query).await.unwrap();

                            sender
                                .send(Message::GetAllBookmarksResponse(bookmarks))
                                .unwrap();
                        }
                        Message::AddBookmarkRequest(url, description) => {
                            let bookmark = client.add_bookmark(&url, &description).await.unwrap();
                            sender.send(Message::AddBookmarkResponse(bookmark)).unwrap();
                        }
                        _ => (),
                    }
                }
            });
        });
    }

    {
        let refresh_button: gtk::Button = workbench::builder().object("refresh").unwrap();
        let sender = sender.clone();

        refresh_button.connect_clicked(move |_| {
            let query = None;
            sender.send(Message::GetAllBookmarksRequest(query)).unwrap();
        });
    }

    {
        let add_button: gtk::Button = workbench::builder().object("add").unwrap();
        let url_entry: adw::EntryRow = workbench::builder().object("url").unwrap();
        let description_text_view: gtk::TextView =
            workbench::builder().object("description").unwrap();

        let sender = sender.clone();

        add_button.connect_clicked(move |_| {
            let url = url_entry.text().to_string();
            if url.is_empty() {
                return;
            }

            let buffer = description_text_view.buffer();
            let (start, end) = buffer.bounds();
            let description = buffer.text(&start, &end, false).to_string();
            buffer.set_text("");

            sender
                .send(Message::AddBookmarkRequest(url, description))
                .unwrap();
        });
    }

    {
        let bookmarks_list: gtk::ListBox = workbench::builder().object("bookmarks").unwrap();

        let on_get_all_bookmarks = move |bookmarks: Vec<Document<Bookmark>>| {
            bookmarks_list.remove_all();

            for bookmark in bookmarks {
                let label = gtk::LinkButton::builder()
                    .label(bookmark.fields.url.clone())
                    .uri(bookmark.fields.url.clone())
                    .build();
                bookmarks_list.append(&label);
            }
        };

        let on_add_bookmark = |bookmark: Document<Bookmark>| {
            println!("{:?}", bookmark);
        };

        let mut receiver = sender.subscribe();

        glib::spawn_future_local(async move {
            while let Ok(message) = receiver.recv().await {
                match message {
                    Message::GetAllBookmarksResponse(bookmarks) => {
                        on_get_all_bookmarks(bookmarks);
                    }
                    Message::AddBookmarkResponse(bookmark) => {
                        on_add_bookmark(bookmark);
                    }
                    _ => (),
                }
            }
        });
    }
}

