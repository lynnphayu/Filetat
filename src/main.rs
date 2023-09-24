use core::time;
use std::{
    cell::RefCell,
    clone,
    collections::HashMap,
    fs::File,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex, RwLock,
    },
    thread::{self, sleep, JoinHandle},
};

enum METHOD {
    GET,
    POST,
}

struct HTTPRequest {
    tcp_stream: TcpStream,
    method: Option<METHOD>,
    path: String,
    query_parameters: Option<Vec<(String, String)>>,
    body: Option<String>,
}

struct Server<'a> {
    listener: &'a TcpListener,
    get_handlers: &'a Arc<RwLock<HashMap<String, fn(HTTPRequest)>>>,
    thread_pool: Option<ThreadPool>,
}

struct ThreadPool {
    current: RefCell<usize>,
    workers: Vec<Worker>,
    senders: Vec<Sender<HTTPRequest>>,
}

struct Worker {
    id: usize,
    join_handle: JoinHandle<()>,
    status: Arc<Mutex<bool>>,
}

impl ThreadPool {
    pub fn new(
        size: usize,
        get_handlers: &Arc<RwLock<HashMap<String, fn(HTTPRequest)>>>,
    ) -> ThreadPool {
        assert!(size > 0);
        let mut workers: Vec<Worker> = Vec::with_capacity(size);
        let mut senders: Vec<Sender<HTTPRequest>> = Vec::with_capacity(size);
        for i in 0..size {
            let (sender, reveiver) = mpsc::channel();
            workers.push(Worker::new(i, reveiver, get_handlers));
            senders.push(sender);
        }
        ThreadPool {
            workers,
            current: RefCell::new(0),
            senders: senders,
        }
    }
    fn execute(&self, request: HTTPRequest) {
        let mut current_index = self.current.clone().into_inner();
        let tx = &self.senders[current_index];
        let status = self.workers[current_index].status.lock().unwrap();
        println!("STATUS ___ {}", status);
        if *status {
            println!("{} {} {}", request.path, current_index, status);
            tx.send(request).unwrap();
        } else {
            current_index = current_index + 1;
            println!("{} {} {}", request.path, current_index, status);
            self.senders[current_index].send(request).unwrap();
        }
        drop(status);
        let next_index = current_index + 1;
        if next_index == self.workers.len() {
            self.current.replace(0);
        } else {
            self.current.replace(next_index);
        }
    }
}

impl Worker {
    fn new(
        id: usize,
        rx: Receiver<HTTPRequest>,
        map: &Arc<RwLock<HashMap<String, fn(HTTPRequest)>>>,
    ) -> Worker {
        let cloned_map = map.clone();
        let status_mutex = Arc::new(Mutex::new(true));
        let status_mutex_copy = status_mutex.clone();

        let join_handle = thread::spawn(move || loop {
            if let Ok(request) = rx.recv_timeout(time::Duration::from_millis(500)) {
                let mut status = status_mutex.lock().unwrap();
                *status = false;
                drop(status);
                match cloned_map.read().unwrap().get(&request.path) {
                    Some(handler) => {
                        let copied_handler = handler.to_owned();
                        copied_handler(request);
                    }
                    None => {
                        println!("NOTFOUND");
                        not_found(request);
                    }
                }
                let mut status = status_mutex.lock().unwrap();
                *status = true;
                drop(status);
            }
        });

        return Worker {
            id,
            join_handle,
            status: status_mutex_copy,
        };
    }
}

impl Server<'_> {
    fn get(&mut self, path: &str, handler: fn(HTTPRequest)) {
        self.get_handlers
            .write()
            .unwrap()
            .insert(String::from(path), handler);
    }

    fn listen(&mut self) {
        let thread_pool = ThreadPool::new(4, self.get_handlers);
        self.thread_pool = Some(thread_pool);
        for stream in self.listener.incoming() {
            match stream {
                Ok(str) => {
                    let request = prepare_request(str);
                    self.thread_pool.as_ref().unwrap().execute(request);
                }
                Err(e) => println!("stream is disrupted: {e}"),
            }
        }

    }
}

fn respond_file<'a>(stream: &'a mut TcpStream, filename: &'a str) -> &'a mut TcpStream {
    match File::open(filename) {
        Ok(file) => {
            stream.write(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
            for byte in file.bytes() {
                stream.write(&[byte.unwrap()]).unwrap();
            }
        }
        Err(e) => {
            if e.kind() == ErrorKind::NotFound {
                stream.write(b"HTTP/1.1 400 OK\r\n\r\n").unwrap();
                stream.write(b"Not Found").unwrap();
            }
        }
    }
    return stream;
}

fn get_profile(mut request: HTTPRequest) {
    respond_file(&mut request.tcp_stream, "public/profile.html")
        .flush()
        .unwrap();
}

fn not_found(mut request: HTTPRequest) {
    sleep(time::Duration::from_secs(10));
    request
        .tcp_stream
        .write(b"HTTP/1.1 404 OK\r\n\r\n")
        .unwrap();
    request.tcp_stream.write(b"Not Found").unwrap();
    request.tcp_stream.flush().unwrap();
}

fn main() {
    let mut server = Server {
        listener: &TcpListener::bind("0.0.0.0:8080").unwrap(),
        get_handlers: &mut Arc::new(RwLock::new(HashMap::new())),
        thread_pool: None,
    };
    server.get("/profile", get_profile);
    server.listen();
}

fn prepare_request(mut stream: TcpStream) -> HTTPRequest {
    let mut req = [0 as u8; 2048];
    if let Err(e) = stream.read(&mut req) {
        println!("req corrupted : {}", e);
    }
    let peer_addr = stream.peer_addr().unwrap().to_string();
    let request = construct_request(stream, &req);
    println!("PEER {} PATH {}", peer_addr, request.path);

    return request;
}

fn construct_request(stream: TcpStream, utf8_arr: &[u8]) -> HTTPRequest {
    let req_string = String::from_utf8_lossy(&utf8_arr[..]).clone();
    let mut req_split_iter = req_string.as_ref().split("\r\n\r\n");
    let headers_string = req_split_iter.next().unwrap();
    let headers: Vec<&str> = headers_string
        .split("\n")
        .next()
        .unwrap()
        .split(" ")
        .collect();

    let method = headers[0];

    let url_string = headers[1];
    let mut url = url_string.split("?");
    let path = String::from(url.next().unwrap());
    let query_string = url.next().unwrap_or("");

    let body = match req_split_iter.next() {
        Some(b) => Some(String::from(b)),
        None => None,
    };

    let query_parameters = if query_string.is_empty() {
        None
    } else {
        Some(Vec::from_iter(query_string.split("&").map(|x| {
            let mut pair = x.split("=");
            return (
                String::from(pair.next().unwrap()),
                String::from(pair.next().unwrap()),
            );
        })))
    };

    let method = match method {
        "GET" => Some(METHOD::GET),
        "POST" => Some(METHOD::POST),
        &_ => None,
    };

    return HTTPRequest {
        tcp_stream: stream,
        method,
        path,
        query_parameters,
        body,
    };
}
