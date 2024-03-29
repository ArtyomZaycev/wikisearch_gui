use std::collections::HashMap;
use std::collections::LinkedList;
use std::fs::File;
use std::io::Write;
use std::mem::swap;
use std::sync::mpsc::Receiver;
use std::sync::mpsc::{self, Sender};
use std::sync::mpsc::TryRecvError;
use std::thread;

use reqwest::blocking::Client;

use crate::bench::Bench;

macro_rules! starts_with_any {
    ($s:expr; $($pats:expr),+) => {
        $($s.starts_with($pats))||+
    };
}

fn get_html(from: &str, client: &mut Client) -> Result<String, Box<dyn std::error::Error>> {
    Ok(client.get(from).send()?.text()?)
}
fn get_html_bench(from: &str, client: &mut Client, bench: &mut Bench) -> Result<String, Box<dyn std::error::Error>> {
    bench.start(0);
    let r = get_html(from, client);
    bench.stop(0);
    r
}

#[allow(dead_code)]
fn get_links(from: &str, client: &mut Client) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let html = get_html(("https://en.wikipedia.org/wiki/".to_string() + &from).as_str(), client)?;

    let beg = html.find("<div id=\"mw-content-text\"");
    if let Some(beg) = beg {
        let mut res = Vec::new();

        let mut x = &html[beg..];
        
        while let Some(ind) = x.find("<a href=\"/wiki/") {
            x = &x[ind + 15..];
            let ref_end = x.find("\"").unwrap();
            let r = &x[..ref_end];
            if starts_with_any!(r; "File:", "Category:", "Special:", "Talk:", "Wikipedia:", "Template:", "Portal:", "Help:") {
                res.push(r.to_string());
            }
            x = &x[ref_end..];
        }

        Ok(res)
    }
    else {
        Ok(vec![])
    }
}
fn get_links_bench(from: &str, client: &mut Client, bench: &mut Bench) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let html = get_html_bench(("https://en.wikipedia.org/wiki/".to_string() + &from).as_str(), client, bench)?;

    bench.start(1);

    let beg = html.find("<div id=\"mw-content-text\"");
    if let Some(beg) = beg {
        let mut res = Vec::new();

        let mut x = &html[beg..];
        
        while let Some(ind) = x.find("<a href=\"/wiki/") {
            x = &x[ind + 15..];
            let ref_end = x.find("\"").unwrap();
            let r = &x[..ref_end];
            if !r.contains(':') {
                res.push(r.to_string());
            }
            x = &x[ref_end..];
        }

        bench.stop(1);
        Ok(res)
    }
    else {
        bench.stop(1);
        Ok(vec![])
    }
}

fn collect_benches(bench_reciever: &Receiver<Bench>) -> Bench {
    let mut benches = Vec::new();
    loop {
        let recv_res = bench_reciever.try_recv();
        if recv_res.is_err() {
            if recv_res.unwrap_err() == TryRecvError::Empty {
                break;
            }
        }
        else {
            benches.push(recv_res.unwrap());
        }
    }

    let mut res = Bench::new();
    for bench in &benches {
        res.combine(bench);
    }
    res
}

fn write_bench_results(bench_results: &Bench, path: &str) {
    let file = File::create(path);

    if file.is_err() {
        eprintln!("Error while creating a bench results file({}): {:?}", path, file.unwrap_err());
    }
    else {
        let mut file = file.unwrap();
        for i in 0..=255u8 {
            let dur = bench_results.get_duration(i);
            if dur.as_nanos() > 0 {
                file.write_all(format!("{}: {}s\n", i, dur.as_secs_f64()).as_bytes()).unwrap();
            }
        }
    }
}

#[derive(PartialEq, Eq)]
enum ThreadState {
    Idle,
    Processing,
    Error,
}

#[allow(dead_code)]
pub fn search(from: &str, to: &str, num_of_threads: usize, max_num_of_links: usize, num_of_links_sender: Sender<(usize, usize, usize)>, dead_threads_sender: Sender<usize>) -> Vec<String> {
    if from == to {
        return vec![from.to_string()];
    }
    let from = &from[from.rfind('/').unwrap() + 1..];
    let to = &to[to.rfind('/').unwrap() + 1..];

    let mut all = HashMap::new();
    all.insert(from.to_string(), "".to_string());

    let mut in_search = LinkedList::new();
    in_search.push_back(from.to_string());

    let mut in_search_next = LinkedList::new();

    let mut txs = Vec::new();
    let mut rxs = Vec::new();
    let mut handlers = Vec::new();
    let mut states = Vec::new();
    let mut plinks: Vec<Option<String>> = Vec::new();

    for _ in 0..num_of_threads {
        let (tx1, rx) = mpsc::channel(); // from main thread
        let (tx, rx1) = mpsc::channel(); // to main thread
        txs.push(tx1);
        rxs.push(rx1);

        handlers.push(thread::spawn(move || {
            let mut client = Client::default();
            
            loop {
                let url = rx.recv();
                if url.is_err() {
                    break;
                }
                let url: String = url.unwrap();
                if url == "kill".to_string() {
                    break;
                }
                if tx.send(get_links(url.as_str(), &mut client).unwrap()).is_err() {
                    break;
                }
            }
        }));

        states.push(ThreadState::Idle);
        plinks.push(None);
    }

    let mut processed = 0usize;

    let mut depth_level = 1usize;

    let mut num_of_links_changed = true;
    // while path betweeen links is not found
    loop {
        // while every link is in_search is not processed
        while !in_search.is_empty() || states.contains(&ThreadState::Processing) {
            if num_of_links_changed {
                let res = num_of_links_sender.send((processed, in_search.len() + in_search_next.len(), depth_level));
                if res.is_err() {
                    eprintln!("Main thread is closed");

                    for tx in txs {
                        let _ = tx.send("kill".to_string());
                    }
                    for handler in handlers {
                        let _ = handler.join();
                    }

                    return vec![];
                }

                num_of_links_changed = false;
            }

            if max_num_of_links > 0 && in_search.len() + in_search_next.len() >= max_num_of_links {
                eprintln!("Max number of links in the queue exceeded");

                for tx in txs {
                    let _ = tx.send("kill".to_string());
                }
                for handler in handlers {
                    let _ = handler.join();
                }

                return vec![];
            }

            for i in 0..num_of_threads {
                if states[i] == ThreadState::Processing {
                    let r = rxs[i].try_recv();

                    match r {
                        Ok(v) => {
                            processed += 1;
                            num_of_links_changed = true;

                            for c in &v {
                                if c == to {
                                    let mut res = vec![];
                    
                                    let mut li = c.clone();
                                    res.push(li.clone());
                                    li = plinks[i].clone().unwrap();
                                    res.push(li.clone());
                    
                                    while li != from {
                                        li = all.get(&li).unwrap().clone();
                                        res.push(li.clone());
                                    }
                                    res.reverse();

                                    for tx in txs {
                                        let _ = tx.send("kill".to_string());
                                    }
                                    for handler in handlers {
                                        let _ = handler.join();
                                    }
                                    return res;
                                }
                                
                                if !all.contains_key(c) {
                                    all.insert(c.clone(), plinks[i].clone().unwrap());
                                    in_search_next.push_back(c.clone());
                                    num_of_links_changed = true;
                                }
                            }

                            states[i] = ThreadState::Idle;
                            plinks[i] = None;
                        },
                        Err(e) => {
                            if e == TryRecvError::Disconnected {
                                states[i] = ThreadState::Error;
                                in_search.push_front(plinks[i].clone().unwrap());
                                num_of_links_changed = true;
                                plinks[i] = None;

                                eprintln!("Thread {} died", i);
                                let _ = dead_threads_sender.send(i);
                            }
                        },
                    }
                }
            }

            for i in 0..num_of_threads {
                if states[i] == ThreadState::Idle {
                    if !in_search.is_empty() {
                        let link = in_search.pop_front().unwrap();
                        num_of_links_changed = true;
                        if txs[i].send(link.clone()).is_err() {
                            states[i] = ThreadState::Error;
                            eprintln!("Error while sending to thread №{}", i);
                        }
                        states[i] = ThreadState::Processing;
                        plinks[i] = Some(link.clone());

                        //println!("List size is {}. Checking {}", in_search.len() + in_search_next.len(), link);
                    }
                }
            }
        }

        swap(&mut in_search, &mut in_search_next);
        depth_level += 1;
    }
}

pub fn search_bench(from: &str, to: &str, num_of_threads: usize, max_num_of_links: usize, num_of_links_sender: Sender<(usize, usize, usize)>, dead_threads_sender: Sender<usize>) -> Vec<String> {
    if from == to {
        return vec![from.to_string()];
    }
    let from = &from[from.rfind('/').unwrap() + 1..];
    let to = &to[to.rfind('/').unwrap() + 1..];

    let mut all = HashMap::new();
    all.insert(from.to_string(), "".to_string());

    let mut in_search = LinkedList::new();
    in_search.push_back(from.to_string());

    let mut in_search_next = LinkedList::new();

    let mut txs = Vec::new();
    let mut rxs = Vec::new();
    let mut handlers = Vec::new();
    let mut states = Vec::new();
    let mut plinks: Vec<Option<String>> = Vec::new();

    let (bench_sender, bench_reciever) = mpsc::channel();

    for _ in 0..num_of_threads {
        let (tx1, rx) = mpsc::channel(); // from main thread
        let (tx, rx1) = mpsc::channel(); // to main thread
        txs.push(tx1);
        rxs.push(rx1);

        let bench_sender = bench_sender.clone();

        handlers.push(thread::spawn(move || {
            let mut client = Client::default();
            
            let mut bench = Bench::new();
            loop {
                let url = rx.recv();
                if url.is_err() {
                    break;
                }
                let url: String = url.unwrap();
                if url == "kill".to_string() {
                    break;
                }
                if tx.send(get_links_bench(url.as_str(), &mut client, &mut bench).unwrap()).is_err() {
                    break;
                }
            }
            bench_sender.send(bench).unwrap();
        }));

        states.push(ThreadState::Idle);
        plinks.push(None);
    }

    let mut processed = 0usize;

    let mut depth_level = 0usize;

    let mut num_of_links_changed = true;
    // while path betweeen links is not found
    loop {
        // while every link is in_search is not processed
        while !in_search.is_empty() || states.contains(&ThreadState::Processing) {
            if num_of_links_changed {
                let res = num_of_links_sender.send((processed, in_search.len() + in_search_next.len(), depth_level));
                if res.is_err() {
                    eprintln!("Main thread is closed");

                    for tx in txs {
                        let _ = tx.send("kill".to_string());
                    }
                    for handler in handlers {
                        let _ = handler.join();
                    }
                    write_bench_results(&collect_benches(&bench_reciever), "bench.txt");

                    return vec![];
                }

                num_of_links_changed = false;
            }

            if max_num_of_links > 0 && in_search.len() + in_search_next.len() >= max_num_of_links {
                eprintln!("Max number of links in the queue exceeded");

                for tx in txs {
                    let _ = tx.send("kill".to_string());
                }
                for handler in handlers {
                    let _ = handler.join();
                }
                write_bench_results(&collect_benches(&bench_reciever), "bench.txt");

                return vec![];
            }

            for i in 0..num_of_threads {
                if states[i] == ThreadState::Processing {
                    let r = rxs[i].try_recv();

                    match r {
                        Ok(v) => {
                            processed += 1;
                            num_of_links_changed = true;

                            for c in &v {
                                if c == to {
                                    let mut res = vec![];
                    
                                    let mut li = c.clone();
                                    res.push(li.clone());
                                    li = plinks[i].clone().unwrap();
                                    res.push(li.clone());
                    
                                    while li != from {
                                        li = all.get(&li).unwrap().clone();
                                        res.push(li.clone());
                                    }
                                    res.reverse();

                                    for tx in txs {
                                        let _ = tx.send("kill".to_string());
                                    }
                                    for handler in handlers {
                                        let _ = handler.join();
                                    }
                                    write_bench_results(&collect_benches(&bench_reciever), "bench.txt");
                                    return res;
                                }
                                
                                if !all.contains_key(c) {
                                    all.insert(c.clone(), plinks[i].clone().unwrap());
                                    in_search_next.push_back(c.clone());
                                    num_of_links_changed = true;
                                }
                            }

                            states[i] = ThreadState::Idle;
                            plinks[i] = None;
                        },
                        Err(e) => {
                            if e == TryRecvError::Disconnected {
                                states[i] = ThreadState::Error;
                                in_search.push_front(plinks[i].clone().unwrap());
                                num_of_links_changed = true;
                                plinks[i] = None;

                                eprintln!("Thread {} died", i);
                                let _ = dead_threads_sender.send(i);
                            }
                        },
                    }
                }
            }

            for i in 0..num_of_threads {
                if states[i] == ThreadState::Idle {
                    if !in_search.is_empty() {
                        let link = in_search.pop_front().unwrap();
                        num_of_links_changed = true;
                        if txs[i].send(link.clone()).is_err() {
                            states[i] = ThreadState::Error;
                            eprintln!("Error while sending to thread №{}", i);
                        }
                        states[i] = ThreadState::Processing;
                        plinks[i] = Some(link.clone());

                        //println!("List size is {}. Checking {}", in_search.len() + in_search_next.len(), link);
                    }
                }
            }
        }

        swap(&mut in_search, &mut in_search_next);
        depth_level += 1;
    }
}