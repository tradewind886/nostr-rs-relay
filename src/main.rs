//! Server process
use futures::SinkExt;
use futures::StreamExt;
use log::*;
use nostr_rs_relay::close::Close;
use nostr_rs_relay::conn;
use nostr_rs_relay::db;
use nostr_rs_relay::error::{Error, Result};
use nostr_rs_relay::event::Event;
use nostr_rs_relay::protostream;
use nostr_rs_relay::protostream::NostrMessage::*;
use nostr_rs_relay::protostream::NostrResponse::*;
use std::collections::HashMap;
use std::env;
use tokio::net::{TcpListener, TcpStream};
use tokio::runtime::Builder;
use tokio::sync::broadcast;
use tokio::sync::broadcast::{Receiver, Sender};
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Start running a Nostr relay server.
fn main() -> Result<(), Error> {
    // setup logger and environment
    let _ = env_logger::try_init();
    let addr = env::args()
        .nth(1)
        .unwrap_or_else(|| "0.0.0.0:8080".to_string());
    // configure tokio runtime
    let rt = Builder::new_multi_thread()
        .enable_all()
        .thread_name("tokio-ws")
        .build()
        .unwrap();
    // start tokio
    rt.block_on(async {
        let listener = TcpListener::bind(&addr).await.expect("Failed to bind");
        info!("listening on: {}", addr);
        // all client-submitted valid events are broadcast to every
        // other client on this channel.  This should be large enough
        // to accomodate slower readers (messages are dropped if
        // clients can not keep up).
        let (bcast_tx, _) = broadcast::channel::<Event>(4096);
        // validated events that need to be persisted are sent to the
        // database on via this channel.
        let (event_tx, event_rx) = mpsc::channel::<Event>(16);
        // start the database writer thread.  Give it a channel for
        // writing events, and for publishing events that have been
        // written (to all connected clients).
        db::db_writer(event_rx, bcast_tx.clone()).await;
        // establish a channel for letting all threads now about a
        // requested server shutdown.
        let (invoke_shutdown, _) = broadcast::channel::<()>(1);
        let ctrl_c_shutdown = invoke_shutdown.clone();
        // listen for ctrl-c interruupts
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.unwrap();
            info!("shutting down due to SIGINT");
            ctrl_c_shutdown.send(()).ok();
        });
        let mut stop_listening = invoke_shutdown.subscribe();
        // handle new client connection requests, or SIGINT signals.
        loop {
            tokio::select! {
                _ = stop_listening.recv() => {
                    break;
                }
                Ok((stream, _)) = listener.accept() => {
                    tokio::spawn(nostr_server(
                        stream,
                        bcast_tx.clone(),
                        event_tx.clone(),
                        invoke_shutdown.subscribe(),
                    ));
                }
            }
        }
    });
    Ok(())
}

/// Handle new client connections.  This runs through an event loop
/// for all client communication.
async fn nostr_server(
    stream: TcpStream,
    broadcast: Sender<Event>,
    event_tx: tokio::sync::mpsc::Sender<Event>,
    mut shutdown: Receiver<()>,
) {
    // get a broadcast channel for clients to communicate on
    let mut bcast_rx = broadcast.subscribe();
    // upgrade the TCP connection to WebSocket
    let conn = tokio_tungstenite::accept_async(stream).await;
    let ws_stream = conn.expect("websocket handshake error");
    // wrap websocket into a stream & sink of Nostr protocol messages
    let mut nostr_stream = protostream::wrap_ws_in_nostr(ws_stream);
    // Track internal client state
    let mut conn = conn::ClientConn::new();
    let cid = conn.get_client_prefix();
    // Create a channel for receiving query results from the database.
    // we will send out the tx handle to any query we generate.
    let (query_tx, mut query_rx) = mpsc::channel::<db::QueryResult>(256);
    // maintain a hashmap of a oneshot channel for active subscriptions.
    // when these subscriptions are cancelled, make a message
    // available to the executing query so it knows to stop.
    //let (abandon_query_tx, _) = oneshot::channel::<()>();
    let mut running_queries: HashMap<String, oneshot::Sender<()>> = HashMap::new();
    // for stats, keep track of how many events the client published,
    // and how many it received from queries.
    let mut client_published_event_count: usize = 0;
    let mut client_received_event_count: usize = 0;
    info!("new connection for client: {}", cid);
    loop {
        tokio::select! {
            _ = shutdown.recv() => {
                // server shutting down, exit loop
                break;
            },
            Some(query_result) = query_rx.recv() => {
                // database informed us of a query result we asked for
                let res = EventRes(query_result.sub_id,query_result.event);
                client_received_event_count += 1;
                nostr_stream.send(res).await.ok();
            },
            Ok(global_event) = bcast_rx.recv() => {
                // an event has been broadcast to all clients
                // first check if there is a subscription for this event.
                let sub_name_opt = conn.get_matching_subscription(&global_event);
                if let Some(sub_name) = sub_name_opt {
                    // TODO: serialize at broadcast time, instead of
                    // once for each consumer.
                    if let Ok(event_str) = serde_json::to_string(&global_event) {
                        debug!("sub match: client: {}, sub: {}, event: {}",
                               cid, sub_name,
                               global_event.get_event_id_prefix());
                        // create an event response and send it
                        let res = EventRes(sub_name.to_owned(),event_str);
                        nostr_stream.send(res).await.ok();
                    } else {
                        warn!("could not convert event to string");
                    }
                }
            },
            // check if this client has a subscription
            proto_next = nostr_stream.next() => {
                match proto_next {
                    Some(Ok(EventMsg(ec))) => {
                        // An EventCmd needs to be validated to be converted into an Event
                        // handle each type of message
                        let parsed : Result<Event> = Result::<Event>::from(ec);
                        match parsed {
                            Ok(e) => {
                                let id_prefix:String = e.id.chars().take(8).collect();
                                debug!("successfully parsed/validated event: {} from client: {}", id_prefix, cid);
                                // Write this to the database
                                event_tx.send(e.clone()).await.ok();
                                client_published_event_count += 1;
                            },
                            Err(_) => {info!("client {} sent an invalid event", cid)}
                        }
                    },
                    Some(Ok(SubMsg(s))) => {
                        debug!("client {} requesting a subscription", cid);
                        // subscription handling consists of:
                        // * registering the subscription so future events can be matched
                        // * making a channel to cancel to request later
                        // * sending a request for a SQL query
                        let (abandon_query_tx, abandon_query_rx) = oneshot::channel::<()>();
                        running_queries.insert(s.id.to_owned(), abandon_query_tx);
                        // register this connection
                        conn.subscribe(s.clone()).ok();
                        // start a database query
                        db::db_query(s, query_tx.clone(), abandon_query_rx).await;
                    },
                    Some(Ok(CloseMsg(cc))) => {
                        // closing a request simply removes the subscription.
                        let parsed : Result<Close> = Result::<Close>::from(cc);
                        match parsed {
                            Ok(c) => {
                                // check if a query is currently
                                // running, and remove it if so.
                                let stop_tx = running_queries.remove(&c.id);
                                if let Some(tx) = stop_tx {
                                    tx.send(()).ok();
                                }
                                // stop checking new events against
                                // the subscription
                                conn.unsubscribe(c);
                            },
                            Err(_) => {info!("invalid command ignored");}
                        }
                    },
                    None => {
                        debug!("normal websocket close from client: {}",cid);
                        break;
                    },
                    Some(Err(Error::ConnError)) => {
                        debug!("got connection close/error, disconnecting client: {}",cid);
                        break;
                    }
                    Some(Err(e)) => {
                        info!("got non-fatal error from client: {}, error: {:?}", cid, e);
                    },
                }
            },
        }
    }
    // connection cleanup - ensure any still running queries are terminated.
    for (_, stop_tx) in running_queries.into_iter() {
        stop_tx.send(()).ok();
    }
    info!(
        "stopping connection for client: {} (client sent {} event(s), received {})",
        cid, client_published_event_count, client_received_event_count
    );
}
