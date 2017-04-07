// Copyright 2017 PingCAP, Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// See the License for the specific language governing permissions and
// limitations under the License.

use std::usize;
use std::sync::{Arc, Mutex};
use std::sync::mpsc::{channel, Sender, Receiver};
use std::thread::{Builder, JoinHandle};
use std::boxed::FnBox;
use super::HashMap;
use std::collections::{BinaryHeap, HashSet};
use std::cmp::Ordering;

struct TaskMeta {
    // the task's number in the pool.
    // each task has a unique number,
    // and it's always bigger than precedes one.
    id: u64,
    // the task's group_id.
    group_id: u64,
    task: Box<FnBox() + Send>,
}

impl TaskMeta {
    fn new<F>(id: u64, group_id: u64, job: F) -> TaskMeta
        where F: FnOnce() + Send + 'static
    {
        TaskMeta {
            id: id,
            group_id: group_id,
            task: Box::new(job),
        }
    }
}

impl<'a> Ord for TaskMeta {
    fn cmp(&self, right: &TaskMeta) -> Ordering {
        if self.id > right.id {
            return Ordering::Less;
        } else if self.id < right.id {
            return Ordering::Greater;
        }
        Ordering::Equal
    }
}

impl PartialEq for TaskMeta {
    fn eq(&self, right: &TaskMeta) -> bool {
        self.cmp(right) == Ordering::Equal
    }
}

impl Eq for TaskMeta {}

impl PartialOrd for TaskMeta {
    fn partial_cmp(&self, rhs: &TaskMeta) -> Option<Ordering> {
        Some(self.cmp(rhs))
    }
}

struct GroupTaskPoolMeta {
    total_tasks_num: u64,
    running_tasks_num: usize,
    waiting_tasks_num: usize,
    // group_id => count
    running_tasks: HashMap<u64, usize>,
    // group_id => tasks array
    waiting_tasks: HashMap<u64, Vec<TaskMeta>>,
    heap: BinaryHeap<TaskMeta>,
    heap_group: HashSet<u64>,
}

impl GroupTaskPoolMeta {
    fn new() -> GroupTaskPoolMeta {
        GroupTaskPoolMeta {
            total_tasks_num: 0,
            running_tasks_num: 0,
            waiting_tasks_num: 0,
            running_tasks: HashMap::default(),
            waiting_tasks: HashMap::default(),
            heap: BinaryHeap::new(),
            heap_group: HashSet::new(),
        }
    }

    // push_task pushes a new task into pool.
    fn push_task<F>(&mut self, group_id: u64, job: F)
        where F: FnOnce() + Send + 'static
    {
        let task = TaskMeta::new(self.total_tasks_num, group_id, job);
        self.waiting_tasks_num += 1;
        self.total_tasks_num += 1;
        let group_id = task.group_id;
        if !self.running_tasks.contains_key(&group_id) && !self.heap_group.contains(&group_id) {
            self.heap_group.insert(group_id);
            self.heap.push(task);
            return;
        }
        let mut group_tasks = self.waiting_tasks
            .entry(group_id)
            .or_insert_with(Vec::new);
        group_tasks.push(task);
    }

    fn total_task_num(&self) -> usize {
        self.waiting_tasks_num + self.running_tasks_num
    }

    // finished_task is called when one task is finished in the
    // thread. It will clean up the remaining information of the
    // task in the pool.
    fn finished_task(&mut self, group_id: u64) {
        self.running_tasks_num -= 1;
        // if running tasks exist.
        if self.running_tasks[&group_id] > 1 {
            let mut count = self.running_tasks.get_mut(&group_id).unwrap();
            *count -= 1;
            return;
        }

        // if no running tasks left.
        self.running_tasks.remove(&group_id);
        if !self.waiting_tasks.contains_key(&group_id) || self.heap_group.contains(&group_id) {
            return;
        }
        // if waiting tasks exist && group not in heap.
        let empty_group_wtasks = {
            let mut waiting_tasks = self.waiting_tasks.get_mut(&group_id).unwrap();
            let front_task = waiting_tasks.swap_remove(0);
            self.heap.push(front_task);
            self.heap_group.insert(group_id);
            waiting_tasks.is_empty()
        };
        if empty_group_wtasks {
            self.waiting_tasks.remove(&group_id);
        }
    }

    // get_next_group returns the next task's group_id.
    // we choose the group according to the following rules:
    // 1. Choose the groups with least running tasks.
    // 2. If more than one groups has the least running tasks,
    //    choose the one whose task comes first(with the minum task's ID)
    fn get_next_group(&self) -> Option<u64> {
        let mut next_group = None; //(group_id,count,task_id)
        for (group_id, tasks) in &self.waiting_tasks {
            let front_task_id = tasks[0].id;
            assert!(self.running_tasks.contains_key(group_id));
            let count = self.running_tasks[group_id];
            if next_group.is_none() {
                next_group = Some((group_id, count, front_task_id));
                continue;
            }
            let (_, pre_count, pre_task_id) = next_group.unwrap();
            if pre_count > count {
                next_group = Some((group_id, count, front_task_id));
                continue;
            }

            if pre_count == count && pre_task_id > front_task_id {
                next_group = Some((group_id, count, front_task_id));
            }

        }

        if let Some((group_id, _, _)) = next_group {
            return Some(*group_id);
        }

        // no tasks in waiting.
        None
    }

    // pop_next_task pops a task to running next.
    // we choose the task according to the following rules:
    // 1. Choose one group to run with func `get_next_group()`.
    // 2. For the tasks in the selected group, choose the first
    // one in the queue(which means comes first).
    fn pop_next_task(&mut self) -> Option<(u64, TaskMeta)> {
        let next_task;
        let group_id;
        if let Some(task) = self.heap.pop() {
            group_id = task.group_id;
            self.heap_group.remove(&group_id);
            next_task = Some((group_id, task));
        } else if let Some(next_group_id) = self.get_next_group() {
            group_id = next_group_id;
            // get front waiting task from group.
            let (empty_group_wtasks, task) = {
                let mut waiting_tasks = self.waiting_tasks.get_mut(&group_id).unwrap();
                let task_meta = waiting_tasks.swap_remove(0);
                (waiting_tasks.is_empty(), task_meta)
            };
            // if waiting tasks for group is empty,remove from waiting_tasks.
            if empty_group_wtasks {
                self.waiting_tasks.remove(&group_id);
            }
            next_task = Some((group_id, task));
        } else {
            return None;
        }
        // running tasks for group add 1.
        self.running_tasks.entry(group_id).or_insert(0 as usize);
        let mut running_tasks = self.running_tasks.get_mut(&group_id).unwrap();
        *running_tasks += 1;
        self.waiting_tasks_num -= 1;
        self.running_tasks_num += 1;
        next_task
    }
}

struct Worker<'a> {
    receiver: &'a Arc<Mutex<Receiver<bool>>>,
    pool_meta: &'a Arc<Mutex<GroupTaskPoolMeta>>,
    running_task_group: Option<u64>,
}

impl<'a> Worker<'a> {
    fn new(receiver: &'a Arc<Mutex<Receiver<bool>>>,
           pool_meta: &'a Arc<Mutex<GroupTaskPoolMeta>>)
           -> Worker<'a> {
        Worker {
            receiver: receiver,
            pool_meta: pool_meta,
            running_task_group: None,
        }
    }

    // wait to receive info from job_receiver,
    // return false when get stop msg
    fn wait(&self) -> bool {
        // try to receive notify
        let job_receiver = self.receiver.lock().unwrap();
        job_receiver.recv().unwrap()
    }

    fn get_next_task(&mut self) -> Option<TaskMeta> {
        // try to get task
        let mut meta = self.pool_meta.lock().unwrap();
        match meta.pop_next_task() {
            Some((group_id, task)) => {
                self.running_task_group = Some(group_id);
                Some(task)
            }
            None => {
                self.running_task_group = None;
                None
            }
        }
    }

    fn process_one_task(&mut self) {
        if let Some(task) = self.get_next_task() {
            task.task.call_box(());
            let mut meta = self.pool_meta.lock().unwrap();
            meta.finished_task(task.group_id);
        }
        self.running_task_group = None;
    }
}


/// A group task pool used to execute tasks in parallel.
/// Spawns `concurrency` threads to process tasks.
/// each task would be pushed into the pool,and when a thread
/// is ready to process a task, it get a task from the waiting queue
/// according the following rules:
///
/// ## Choose one group
/// 1. Choose the groups with least running tasks.
/// 2. If more than one groups has the least running tasks,
///    choose the one whose task comes first(with the minum task's ID)
///
/// ## Chose a task in selected group.
/// For the tasks in the selected group, choose the first
/// one in the queue(which means comes first).
///
/// # Example
///
/// One group with 10 tasks, and 10 groups with 1 task each. Every
/// task cost the same time.
///
/// ```
/// # extern crate tikv;
/// use tikv::util::group_pool::GroupTaskPool;
/// use std::thread::sleep;
/// use std::time::Duration;
/// use std::sync::mpsc::{Sender, Receiver, channel};
/// # fn main(){
/// let concurrency = 2;
/// let name = Some(String::from("sample"));
/// let mut task_pool = GroupTaskPool::new(name,concurrency);
/// let (jtx, jrx): (Sender<u64>, Receiver<u64>) = channel();
/// let group_with_many_tasks = 1001 as u64;
/// // all tasks cost the same time.
/// let sleep_duration = Duration::from_millis(100);
///
/// // push tasks of group_with_many_tasks's job to make all thread in pool busy.
/// // in order to make sure the thread would be free in sequence,
/// // these tasks should run in sequence.
/// for _ in 0..concurrency {
///     task_pool.execute(group_with_many_tasks, move || {
///         sleep(sleep_duration);
///     });
///     // make sure the pre task is running now.
///     sleep(sleep_duration / 4);
/// }
///
/// // push 10 tasks of group_with_many_tasks's job into pool.
/// for _ in 0..10 {
///     let sender = jtx.clone();
///     task_pool.execute(group_with_many_tasks, move || {
///         sleep(sleep_duration);
///         sender.send(group_with_many_tasks).unwrap();
///     });
/// }
///
/// // push 1 task for each group_id in [0..10) into pool.
/// for group_id in 0..10 {
///     let sender = jtx.clone();
///     task_pool.execute(group_id, move || {
///         sleep(sleep_duration);
///         sender.send(group_id).unwrap();
///     });
/// }
/// // when first thread is free, since there would be another
/// // thread running group_with_many_tasks's task,and the first task which
/// // is not belong to group_with_many_tasks in the waitting queue would run first.
/// // when the second thread is free, since there is group_with_many_tasks's task
/// // is running, and the first task in the waiting queue is the
/// // group_with_many_tasks's task,so it would run. Similarly when the first is
/// // free again,it would run the task in front of the queue..
/// for id in 0..10 {
///     let first = jrx.recv().unwrap();
///     assert_eq!(first, id);
///     let second = jrx.recv().unwrap();
///     assert_eq!(second, group_with_many_tasks);
/// }
/// # }
/// ```
pub struct GroupTaskPool {
    tasks: Arc<Mutex<GroupTaskPoolMeta>>,
    job_sender: Sender<bool>,
    threads: Vec<JoinHandle<()>>,
}

impl GroupTaskPool {
    pub fn new(name: Option<String>, num_threads: usize) -> GroupTaskPool {
        assert!(num_threads >= 1);
        let (jtx, jrx) = channel();
        let jrx = Arc::new(Mutex::new(jrx));
        let tasks = Arc::new(Mutex::new(GroupTaskPoolMeta::new()));

        let mut threads = Vec::new();
        // Threadpool threads
        for _ in 0..num_threads {
            threads.push(new_thread(name.clone(), jrx.clone(), tasks.clone()));
        }

        GroupTaskPool {
            tasks: tasks.clone(),
            job_sender: jtx,
            threads: threads,
        }
    }

    /// Executes the function `job` on a thread in the pool.
    pub fn execute<F>(&mut self, group_id: u64, job: F)
        where F: FnOnce() + Send + 'static
    {
        {
            let lock = self.tasks.clone();
            let mut meta = lock.lock().unwrap();
            meta.push_task(group_id, job);
        }
        self.job_sender.send(true).unwrap();
    }

    pub fn get_tasks_num(&self) -> usize {
        let lock = self.tasks.clone();
        let meta = lock.lock().unwrap();
        meta.total_task_num()
    }

    pub fn stop(&mut self) -> Result<(), String> {
        for _ in 0..self.threads.len() {
            if let Err(e) = self.job_sender.send(false) {
                return Err(format!("{:?}", e));
            }
        }
        while let Some(t) = self.threads.pop() {
            if let Err(e) = t.join() {
                return Err(format!("{:?}", e));
            }
        }
        Ok(())
    }
}

fn new_thread(name: Option<String>,
              receiver: Arc<Mutex<Receiver<bool>>>,
              tasks: Arc<Mutex<GroupTaskPoolMeta>>)
              -> JoinHandle<()> {
    let mut builder = Builder::new();
    if let Some(ref name) = name {
        builder = builder.name(name.clone());
    }

    builder.spawn(move || {
            let mut worker = Worker::new(&receiver, &tasks);
            // start the worker.
            // loop break on receive stop message.
            while worker.wait() {
                // handle task
                // since `tikv` would be down on any panic happens,
                // we needn't process panic case here.
                worker.process_one_task();
            }
        })
        .unwrap()
}

#[cfg(test)]
mod test {
    use super::GroupTaskPool;
    use std::thread::sleep;
    use std::time::Duration;
    use std::sync::mpsc::channel;

    #[test]
    fn test_tasks_with_same_cost() {
        let name = Some(thd_name!("test_tasks_with_same_cost"));
        let concurrency = 2;
        let mut task_pool = GroupTaskPool::new(name, concurrency);
        let (jtx, jrx) = channel();
        let group_with_many_tasks = 1001 as u64;
        // all tasks cost the same time.
        let sleep_duration = Duration::from_millis(50);
        let recv_timeout_duration = Duration::from_secs(2);

        // push tasks of group_with_many_tasks's job to make all thread in pool busy.
        // in order to make sure the thread would be free in sequence,
        // these tasks should run in sequence.
        for _ in 0..concurrency {
            task_pool.execute(group_with_many_tasks, move || {
                sleep(sleep_duration);
            });
            // make sure the pre task is running now.
            sleep(sleep_duration / 4);
        }

        // push 10 tasks of group_with_many_tasks's job into pool.
        for _ in 0..10 {
            let sender = jtx.clone();
            task_pool.execute(group_with_many_tasks, move || {
                sleep(sleep_duration);
                sender.send(group_with_many_tasks).unwrap();
            });
        }

        // push 1 task for each group_id in [0..10) into pool.
        for group_id in 0..10 {
            let sender = jtx.clone();
            task_pool.execute(group_id, move || {
                sleep(sleep_duration);
                sender.send(group_id).unwrap();
            });
        }
        // when first thread is free, since there would be another
        // thread running group_with_many_tasks's task,and the first task which
        // is not belong to group_with_many_tasks in the waitting queue would run first.
        // when the second thread is free, since there is group_with_many_tasks's task
        // is running, and the first task in the waiting queue is the
        // group_with_many_tasks's task,so it would run. Similarly when the first is
        // free again,it would run the task in front of the queue..
        for id in 0..10 {
            let first = jrx.recv_timeout(recv_timeout_duration).unwrap();
            assert_eq!(first, id);
            let second = jrx.recv_timeout(recv_timeout_duration).unwrap();
            assert_eq!(second, group_with_many_tasks);
        }
        task_pool.stop().unwrap();
    }


    #[test]
    fn test_tasks_with_different_cost() {
        let name = Some(thd_name!("test_tasks_with_different_cost"));
        let concurrency = 2;
        let mut task_pool = GroupTaskPool::new(name, concurrency);
        let (jtx, jrx) = channel();
        let group_with_big_task = 1001 as u64;
        let sleep_duration = Duration::from_millis(50);
        let recv_timeout_duration = Duration::from_secs(2);

        // push big task into pool.
        task_pool.execute(group_with_big_task, move || {
            sleep(sleep_duration * 10);
        });
        // make sure the big task is running.
        sleep(sleep_duration / 4);

        // push 1 task for each group_id in [0..10) into pool.
        for group_id in 0..10 {
            let sender = jtx.clone();
            task_pool.execute(group_id, move || {
                sleep(sleep_duration);
                sender.send(group_id).unwrap();
            });
        }
        // push 10 tasks of group_with_big_task's job into pool.
        for _ in 0..10 {
            let sender = jtx.clone();
            task_pool.execute(group_with_big_task, move || {
                sleep(sleep_duration);
                sender.send(group_with_big_task).unwrap();
            });
        }

        // since there exist a long task of group_with_big_task is runnging,
        // the other thread would always run other group's tasks in sequence.
        for id in 0..10 {
            let group_id = jrx.recv_timeout(recv_timeout_duration).unwrap();
            assert_eq!(group_id, id);
        }

        for _ in 0..10 {
            let second = jrx.recv_timeout(recv_timeout_duration).unwrap();
            assert_eq!(second, group_with_big_task);
        }
        task_pool.stop().unwrap();
    }
}
