use core::time;
use std::{
    cell::RefCell,
    collections::HashMap,
    fs::File,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc,
    },
    thread::{self, sleep, JoinHandle},
};

type Job = Box<dyn FnOnce() + Send>;

struct HTTPRequest {
    tcp_stream: TcpStream,
    method: String,
    path: String,
    query_parameters: Option<Vec<(String, String)>>,
    body: Option<String>,
}

type HandlerMap = HashMap<String, HashMap<String, fn(HTTPRequest)>>;

struct Server<'a> {
    listener: &'a TcpListener,
    get_handlers: HandlerMap,
    thread_pool: Option<ThreadPool>,
}

struct ThreadPool {
    current: RefCell<usize>,
    workers: Vec<Worker>,
    senders: Vec<Sender<Job>>,
}

struct Worker {
    join_handle: JoinHandle<()>,
    status: Arc<AtomicBool>,
}

impl ThreadPool {
    pub fn new(size: usize) -> ThreadPool {
        assert!(size > 0);
        let mut workers: Vec<Worker> = Vec::with_capacity(size);
        let mut senders: Vec<Sender<Job>> = Vec::with_capacity(size);
        for _ in 0..size {
            let (sender, receiver) = mpsc::channel();
            workers.push(Worker::new(receiver));
            senders.push(sender);
        }
        ThreadPool {
            workers,
            current: RefCell::new(0),
            senders: senders,
        }
    }
    fn execute<F>(&self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        let mut current_index = self.current.clone().into_inner();
        let tx = &self.senders[current_index];
        let status = self.workers[current_index].status.load(Ordering::Relaxed);
        if status {
            println!("{} {} ", current_index, status);
            tx.send(Box::new(f)).unwrap();
        } else {
            current_index = current_index + 1;
            println!("{} {} ", current_index, status);
            self.senders[current_index].send(Box::new(f)).unwrap();
        }
        let next_index = current_index + 1;
        if next_index == self.workers.len() {
            self.current.replace(0);
        } else {
            self.current.replace(next_index);
        }
    }
}

impl Worker {
    fn new(rx: Receiver<Job>) -> Worker {
        let status_mutex = Arc::new(AtomicBool::new(true));
        let status_mutex_copy = status_mutex.clone();

        let join_handle = thread::spawn(move || loop {
            if let Ok(job) = rx.recv_timeout(time::Duration::from_millis(500)) {
                status_mutex.store(false, Ordering::Relaxed);
                job();
                status_mutex.store(true, Ordering::Relaxed)
            }
        });

        return Worker {
            join_handle,
            status: status_mutex_copy,
        };
    }
}

impl Server<'_> {
    fn get(&mut self, path: &str, handler: fn(HTTPRequest)) {
        let get_map = self.get_handlers.get_mut("GET").unwrap();
        get_map.insert(String::from(path), handler);
    }

    fn post(&mut self, path: &str, handler: fn(HTTPRequest)) {
        let get_map = self.get_handlers.get_mut("POST").unwrap();
        get_map.insert(String::from(path), handler);
    }

    fn listen(&mut self) {
        let thread_pool = ThreadPool::new(4);
        self.thread_pool = Some(thread_pool);
        for stream in self.listener.incoming() {
            match stream {
                Ok(str) => {
                    let request = prepare_request(str);
                    let handle = match self
                        .get_handlers
                        .get(&request.method)
                        .unwrap()
                        .get(&request.path)
                        .cloned()
                    {
                        Some(handle) => handle,
                        None => not_found,
                    };
                    self.thread_pool.as_ref().unwrap().execute(move || {
                        handle(request);
                    })
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
    let get: HashMap<String, fn(HTTPRequest)> = HashMap::new();
    let post: HashMap<String, fn(HTTPRequest)> = HashMap::new();
    let mut server = Server {
        listener: &TcpListener::bind("0.0.0.0:8080").unwrap(),
        get_handlers: HashMap::from([(String::from("GET"), get), (String::from("POST"), post)]),
        thread_pool: None,
    };
    server.get("/profile", get_profile);
    server.post("/profile", post_profile);
    server.listen();
}

fn post_profile(mut request: HTTPRequest) {
    let body = request.body.unwrap().to_owned();
    println!("{}", body);
    request
        .tcp_stream
        .write(b"HTTP/1.1 200 OK\nContent-type: application/json\r\n\r\n")
        .unwrap();
    request.tcp_stream.write(body.as_bytes()).unwrap();
    request.tcp_stream.flush().unwrap();
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

    return HTTPRequest {
        tcp_stream: stream,
        method: String::from(method),
        path,
        query_parameters,
        body,
    };
}
