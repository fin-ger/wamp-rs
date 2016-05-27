use websocket::{self, Sender, Receiver};
use websocket::client::request::Url;
use websocket::client;
use websocket::stream;
use websocket::message::{Message as WSMessage, Type};
use websocket::header;
use messages::{URI, Dict, List, ID, SubscribeOptions, PublishOptions, Message,  HelloDetails, Reason, ErrorDetails, ClientRoles};
use std::collections::HashMap;
use serde_json;
use serde::{Deserialize, Serialize};
use std::str::from_utf8;
use std::fmt;
use std::time::Duration;
use ::{WampResult, Error, ErrorKind};
use std::thread::{self, JoinHandle};
use std::sync::{Mutex, Arc};
use rmp_serde::Deserializer as RMPDeserializer;
use rmp_serde::Serializer;
use utils::StructMapWriter;
use std::io::Cursor;
use rmp_serde::encode::VariantWriter;
use eventual::{Complete, Future, Async};

macro_rules! try_websocket {
    ($e: expr) => (
        match $e {
            Ok(result) => result,
            Err(e) => return Err(Error::new(ErrorKind::WebSocketError(e)))
        }
    );
}

pub struct Connection {
    // sender: client::Sender<stream::WebSocketStream>,
    // receiver: client::Receiver<stream::WebSocketStream>,
    realm: URI,
    url: String
}

pub struct Subscription {
    pub topic: URI,
    subscription_id: ID
}

struct CallbackWrapper {
    callback: Box<Fn(List, Dict)>
}

static WAMP_JSON:&'static str = "wamp.2.json";
static WAMP_MSGPACK:&'static str = "wamp.2.msgpack";

#[derive(PartialEq)]
enum ConnectionState {
    Connected,
    ShuttingDown,
    Disconnected
}

unsafe impl <'a> Send for ConnectionInfo {}

unsafe impl<'a> Sync for ConnectionInfo {}

unsafe impl <'a> Send for CallbackWrapper {}

unsafe impl<'a> Sync for CallbackWrapper {}

pub struct Client {
    connection_info: Arc<ConnectionInfo>,
    max_session_id: ID,
    id: u64
}

struct ConnectionInfo {
    connection_state: Mutex<ConnectionState>,
    sender: Mutex<client::Sender<stream::WebSocketStream>>,
    subscription_requests: Mutex<HashMap<ID, Complete<(ID, Arc<ConnectionInfo>), Error>>>,
    unsubscription_requests: Mutex<HashMap<ID, Complete<Arc<ConnectionInfo>, Error>>>,
    subscriptions: Mutex<HashMap<ID, CallbackWrapper>>,
    publish_ids_to_topics: Mutex<HashMap<ID, URI>>,
    protocol: String,
    published_callbacks: Mutex<Vec<Box<Fn(&URI)>>>,
    shutdown_complete: Mutex<Option<Complete<(), Error>>>
}

fn send_message(sender: &Mutex<client::Sender<stream::WebSocketStream>>, message: Message, protocol: &str) -> WampResult<()> {
    debug!("Sending message {:?}", message);
    if protocol == WAMP_MSGPACK {
        send_message_msgpack(sender, message)
    } else {
        send_message_json(sender, message)
    }
}

fn send_message_json(sender: &Mutex<client::Sender<stream::WebSocketStream>>, message: Message) -> WampResult<()> {
    let mut sender = sender.lock().unwrap();
    // Send the message
    match sender.send_message(&WSMessage::text(serde_json::to_string(&message).unwrap())) {
        Ok(()) => Ok(()),
        Err(e) => {
            error!("Could not send messsage: {}", e.to_string());
            let _ = sender.send_message(&WSMessage::close());
            Err(Error::new(ErrorKind::WebSocketError(e)))
        }
    }
}

fn send_message_msgpack(sender: &Mutex<client::Sender<stream::WebSocketStream>>, message: Message) -> WampResult<()> {
    let mut sender = sender.lock().unwrap();

    // Send the message
    let mut buf: Vec<u8> = Vec::new();
    message.serialize(&mut Serializer::with(&mut buf, StructMapWriter)).unwrap();
    match sender.send_message(&WSMessage::binary(buf)) {
        Ok(()) => Ok(()),
        Err(e) => {
            error!("Could not send messsage: {}", e.to_string());
            let _ = sender.send_message(&WSMessage::close());
            Err(Error::new(ErrorKind::WebSocketError(e)))
        }
    }
}

fn handle_welcome_message(receiver: &mut client::Receiver<stream::WebSocketStream>, sender: &Mutex<client::Sender<stream::WebSocketStream>>) -> WampResult<Message> {

    for message in receiver.incoming_messages() {
        let message: WSMessage = try_websocket!(message);
        match message.opcode {
            Type::Close => {
                info!("Received close message, shutting down");
                return Err(Error::new(ErrorKind::ConnectionLost));
            },
            Type::Text => {
                debug!("Recieved welcome message in text form: {:?}", message.payload);
                match from_utf8(&message.payload) {
                    Ok(message_text) => {
                        match serde_json::from_str(message_text) {
                            Ok(message) => {
                                return Ok(message);
                            } Err(e) => {
                                return Err(Error::new(ErrorKind::JSONError(e)));
                            }
                        }
                    },
                    Err(_) => {
                        return Err(Error::new(ErrorKind::MalformedData));
                    }
                }
            },
            Type::Binary => {
                debug!("Recieved welcome message in binary form: {:?}", message.payload);
                let mut de = RMPDeserializer::new(Cursor::new(&*message.payload));
                match Deserialize::deserialize(&mut de) {
                    Ok(message) => {
                        return Ok(message);
                    },
                    Err(e) => {
                        return Err(Error::new(ErrorKind::MsgPackError(e)));
                    }
                }
            },
            Type::Ping => {
                info!("Receieved ping.  Ponging");
                let mut sender = sender.lock().unwrap();
                let _ = sender.send_message(&WSMessage::pong(message.payload));
            },
            Type::Pong => {
                info!("Receieved pong");
            }
        };
    }
    Err(Error::new(ErrorKind::ConnectionLost))
}

impl Connection {
    pub fn new(url: &str, realm: &str) -> Connection {
        Connection {
            realm: URI::new(realm),
            url: url.to_string()
        }
    }

    pub fn connect<'a>(&self) -> WampResult<Client> {
        let url = match Url::parse(&self.url) {
            Ok(url) => url,
            Err(e) => return Err(Error::new(ErrorKind::URLError(e)))
        };
        let mut request = try_websocket!(websocket::Client::connect(url)); // Connect to the server
        request.headers.set(header::WebSocketProtocol(vec![WAMP_MSGPACK.to_string(), WAMP_JSON.to_string()]));
        let response = try_websocket!(request.send()); // Send the request

        try_websocket!(response.validate()); // Ensure the response is valid
        let protocol = match response.protocol() {
            Some(protocols) => {
                if protocols.len() == 0 {
                    warn!("Router did not specify protocol. Defaulting to wamp.2.json");
                    WAMP_JSON.to_string()
                } else {
                    protocols[0].clone()
                }
            }
            None => {
                warn!("Router did not specify protocol. Defaulting to wamp.2.json");
                WAMP_JSON.to_string()
            }
        };
        let (sender, mut receiver)  = response.begin().split(); // Get a Client

        let info = Arc::new(ConnectionInfo {
            protocol: protocol,
            subscription_requests: Mutex::new(HashMap::new()),
            unsubscription_requests: Mutex::new(HashMap::new()),
            subscriptions: Mutex::new(HashMap::new()),
            sender: Mutex::new(sender),
            publish_ids_to_topics: Mutex::new(HashMap::new()),
            connection_state: Mutex::new(ConnectionState::Connected),
            published_callbacks: Mutex::new(Vec::new()),
            shutdown_complete: Mutex::new(None)
        });


        let hello_message = Message::Hello(self.realm.clone(), HelloDetails::new(ClientRoles::new()));
        debug!("Sending Hello message");
        thread::sleep(Duration::from_millis(200));
        send_message(&info.sender, hello_message, &info.protocol).unwrap();
        debug!("Awaiting welcome message");
        let welcome_message = try!(handle_welcome_message(&mut receiver, &info.sender));
        let session_id = match welcome_message {
            Message::Welcome(session_id, _) => session_id,
            Message::Abort(_, reason) => {
                error!("Recieved abort message.  Reason: {:?}", reason);
                return Err(Error::new(ErrorKind::ConnectionLost));
            },
            _ => return Err(Error::new(ErrorKind::UnexpectedMessage("Expected Welcome Message")))
        };


        self.start_recv_loop(receiver, info.clone());

        Ok(Client {
            connection_info: info,
            id: session_id,
            max_session_id: 0,
        })
    }

    fn start_recv_loop(&self, mut receiver: client::Receiver<stream::WebSocketStream>, mut connection_info: Arc<ConnectionInfo>) -> JoinHandle<()> {
        thread::spawn(move || {
            // Receive loop
            for message in receiver.incoming_messages() {
                let message: WSMessage = match message {
                    Ok(m) => m,
                    Err(e) => {
                        error!("Could not receieve message: {:?}", e);
                        let _ = connection_info.sender.lock().unwrap().send_message(&WSMessage::close());
                        break;
                    }
                };
                match message.opcode {
                    Type::Close => {
                        info!("Received close message, shutting down");
                        let _ = connection_info.sender.lock().unwrap().send_message(&WSMessage::close());
                        break;
                    },
                    Type::Text => {
                        match from_utf8(&message.payload) {
                            Ok(message_text) => {
                                match serde_json::from_str(message_text) {
                                    Ok(message) => {
                                        if !Connection::handle_message(message, &mut connection_info) {
                                            break;
                                        }
                                    } Err(_) => {
                                        error!("Received unknown message: {}", message_text)
                                    }
                                }
                            },
                            Err(_) => {
                                error!("Receieved non-utf-8 json message.  Ignoring");
                            }
                        }
                    },
                    Type::Binary => {
                        let mut de = RMPDeserializer::new(Cursor::new(&*message.payload));
                        match Deserialize::deserialize(&mut de) {
                            Ok(message) => {
                                if !Connection::handle_message(message, &mut connection_info) {
                                    break;
                                }
                            },
                            Err(_) => {
                                error!("Could not understand MsgPack message");
                            }
                        }
                    },
                    Type::Ping => {
                        info!("Receieved ping.  Ponging");
                        let _ = connection_info.sender.lock().unwrap().send_message(&WSMessage::pong(message.payload));
                    },
                    Type::Pong => {
                        info!("Receieved pong");
                    }
                }
            }
            *connection_info.connection_state.lock().unwrap() = ConnectionState::Disconnected;
            {
                let mut sender = connection_info.sender.lock().unwrap();
                let _ = sender.send_message(&WSMessage::close()).unwrap();
                sender.shutdown().ok();
            }
            receiver.shutdown().ok();
            match connection_info.shutdown_complete.lock().unwrap().take() {
                Some(promise) => {
                    promise.complete(());
                },
                None => {}
            };
        })
    }

    fn handle_message(message: Message, connection_info: &mut Arc<ConnectionInfo>) -> bool {
        match message {
            Message::Subscribed(request_id, subscription_id) => {
                // TODO handle errors here
                info!("Recieved a subscribed notification");
                match connection_info.subscription_requests.lock().unwrap().remove(&request_id) {
                    Some(promise) => {
                        debug!("Completing promise");
                        promise.complete((subscription_id, connection_info.clone()))
                    },
                    None => {
                        warn!("Recieved a subscribed notification for a subscription we don't have.  ID: {}", request_id);
                    }
                }

            },
            Message::Unsubscribed(request_id) => {
                match connection_info.unsubscription_requests.lock().unwrap().remove(&request_id) {
                    Some(promise) => {
                        promise.complete(connection_info.clone())
                    },
                    None => {
                        warn!("Recieved a unsubscribed notification for a subscription we don't have.  ID: {}", request_id);
                    }
                }
            },
            Message::Event(subscription_id, _, _, args, kwargs) => {
                let args = args.unwrap_or(Vec::new());
                let kwargs = kwargs.unwrap_or(HashMap::new());
                match connection_info.subscriptions.lock().unwrap().get(&subscription_id) {
                    Some(subscription) => {
                        let ref callback = subscription.callback;
                        callback(args, kwargs);
                    },
                    None => {
                        warn!("Recieved an event for a subscription we don't have.  ID: {}", subscription_id);
                    }
                }
            },
            Message::Published(request_id, _publication_id) => {
                let ids = connection_info.publish_ids_to_topics.lock().unwrap();
                match ids.get(&request_id) {
                    Some(ref topic) => {
                        for callback in connection_info.published_callbacks.lock().unwrap().iter() {
                            callback(topic);
                        }
                    },
                    None => {}
                }

            }
            Message::Goodbye(_, reason) => {
                match *connection_info.connection_state.lock().unwrap() {
                    ConnectionState::Connected => {
                        info!("Router said goodbye.  Reason: {:?}", reason);
                        send_message(&connection_info.sender, Message::Goodbye(ErrorDetails::new(), Reason::GoodbyeAndOut), &connection_info.protocol).unwrap();
                        return false;
                    },
                    ConnectionState::ShuttingDown => {
                        // The router has seen our goodbye message and has responded in kind
                        info!("Router acknolwedged disconnect");
                        match connection_info.shutdown_complete.lock().unwrap().take() {
                            Some(promise) => promise.complete(()),
                            None          => {}
                        }
                        return false;
                    },
                    ConnectionState::Disconnected => {
                        // Should never happen
                        return false;
                    }
                }
            }
            _ => {}
        }
        true
    }
}



impl Client {

    fn send_message(&self, message: Message) -> WampResult<()> {
        if self.connection_info.protocol == WAMP_MSGPACK {
            send_message_msgpack(&self.connection_info.sender, message)
        } else {
            send_message_json(&self.connection_info.sender, message)
        }
    }

    fn get_next_session_id(&mut self) -> ID {
        self.max_session_id += 1;
        self.max_session_id
    }

    pub fn subscribe(&mut self, topic: URI, callback: Box<Fn(List, Dict)>) -> WampResult<Future<Subscription, Error>> {
        // Send a subscribe messages
        let request_id = self.get_next_session_id();
        let (complete, future) = Future::<(ID, Arc<ConnectionInfo>), Error>::pair();
        let the_topic = topic.clone();
        let callback = CallbackWrapper {callback: callback};
        let future = future.and_then(move |(subscription_id, info): (ID, Arc<ConnectionInfo>)| {
            debug!("Inserting subscription");
            info.subscriptions.lock().unwrap().insert(subscription_id, callback);
             Ok(Subscription{topic: the_topic, subscription_id: subscription_id})
        });
        self.connection_info.subscription_requests.lock().unwrap().insert(request_id, complete);
        try!(self.send_message(Message::Subscribe(request_id, SubscribeOptions::new(), topic)));
        Ok(future)
    }

    pub fn unsubscribe(&mut self, subscription: Subscription) -> WampResult<Future<(), Error>> {
        self.connection_info.subscription_requests.lock().unwrap();

        let request_id = self.get_next_session_id();
        try!(self.send_message(Message::Unsubscribe(request_id, subscription.subscription_id)));
        let (complete, future) = Future::<Arc<ConnectionInfo>, Error>::pair();
        self.connection_info.unsubscription_requests.lock().unwrap().insert(request_id, complete);
        Ok(future.and_then(move |info| {
            info.subscriptions.lock().unwrap().remove(&subscription.subscription_id);
            Ok(())
        }))
    }

    pub fn on_published(&mut self, callback: Box<Fn(&URI)>) {
        self.connection_info.published_callbacks.lock().unwrap().push(callback);
    }

    pub fn publish(&mut self, topic: URI, args: Option<List>, kwargs: Option<Dict>) -> WampResult<()> {
        info!("Publishing to {:?} with {:?} | {:?}", topic, args, kwargs);
        let request_id = self.get_next_session_id();
        let request_acknowledge = self.connection_info.published_callbacks.lock().unwrap().len() > 0;
        if request_acknowledge {
            debug!("Requesting acknowledgement");
            let mut ids = self.connection_info.publish_ids_to_topics.lock().unwrap();
            ids.insert(request_id, topic.clone());
        }
        self.send_message(Message::Publish(request_id, PublishOptions::new(request_acknowledge), topic, args, kwargs))
    }

    pub fn shutdown(&mut self) -> WampResult<Future<(), Error>> {
        let mut state = self.connection_info.connection_state.lock().unwrap();
        if *state == ConnectionState::Connected {
            *state = ConnectionState::ShuttingDown;
            let (complete, future) = Future::pair();
            *self.connection_info.shutdown_complete.lock().unwrap() = Some(complete);
            // TODO add timeout in case server doesn't respond.
            try!(self.send_message(Message::Goodbye(ErrorDetails::new(), Reason::SystemShutdown)));
            Ok(future)
        } else {
            Err(Error::new(ErrorKind::InvalidState("Tried to shut down a client that was already shutting down".to_string())))
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{{Connection id: {}}}", self.id)
    }
}
