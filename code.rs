use adw::prelude::*;
use anyhow::{anyhow, Result};
use aquadoggo::{Configuration, Node};
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
    "bookmarks_002005b6b965fb55d0ec5530a4d4874646b90669eaf72c14fa0c045f8bfaa6c4383f";

#[derive(Deserialize, Clone, Debug)]
struct BookmarksCollection {
    pub bookmarks: Collection<Bookmark>,
}

#[derive(Deserialize, Clone, Debug)]
struct Bookmark {
    pub url: String,
    pub description: String,
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

    pub async fn all_bookmarks(&self) -> Result<Vec<Document<Bookmark>>> {
        let query = format!(
            r#"
            {{
                bookmarks: all_{BOOKMARKS_SCHEMA_ID} {{
                    documents {{
                        meta {{
                            documentId
                            viewId
                            owner
                        }}
                        fields {{
                            url
                            description
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
        let schema_id = SchemaId::new(BOOKMARKS_SCHEMA_ID).unwrap();

        let operation = OperationBuilder::new(&schema_id)
            .fields(&[("url", url.into()), ("description", description.into())])
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
            },
        };

        Ok(document)
    }
}

type Url = String;

type Description = String;

#[derive(Clone, Debug)]
enum Message {
    GetAllBookmarksRequest,
    GetAllBookmarksResponse(Vec<Document<Bookmark>>),
    AddBookmarkRequest(Url, Description),
    AddBookmarkResponse(Document<Bookmark>),
}

pub fn main() {
    let (sender, _) = broadcast::channel::<Message>(16);

    {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();

        let key_pair = KeyPair::new();
        let config = Configuration::default();
        let node = rt.block_on(Node::start(key_pair, config));

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
                        Message::GetAllBookmarksRequest => {
                            let bookmarks = client.all_bookmarks().await.unwrap();
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
            sender.send(Message::GetAllBookmarksRequest).unwrap();
        });
    }

    {
        let add_button: gtk::Button = workbench::builder().object("add").unwrap();
        let url_entry: gtk::Entry = workbench::builder().object("url").unwrap();
        let description_text_view: gtk::TextView =
            workbench::builder().object("description").unwrap();

        let sender = sender.clone();

        add_button.connect_clicked(move |_| {
            let buffer = url_entry.buffer();
            let url = buffer.text().to_string();
            if url.is_empty() {
                return;
            }
            buffer.set_text("");

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
        let mut receiver = sender.subscribe();

        glib::spawn_future_local(async move {
            while let Ok(message) = receiver.recv().await {
                match message {
                    Message::GetAllBookmarksResponse(bookmarks) => {
                        println!("{:?}", bookmarks);
                    }
                    Message::AddBookmarkResponse(bookmark) => {
                        println!("{:?}", bookmark);
                    }
                    _ => (),
                }
            }
        });
    }
}

