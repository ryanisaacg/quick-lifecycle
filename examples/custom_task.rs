
use blinds::{run, EventStream, Event as BlindsEvent, Settings, Window};
use futures_util::task::LocalSpawnExt;
use futures_core::stream::Stream;
use futures_util::stream::FuturesUnordered;
use std::sync::Arc;
use std::cell::RefCell;
use std::pin::Pin;
use futures_util::future::{poll_fn, pending};
use futures_util::future::{select_all, FutureExt};
use std::collections::VecDeque;
use std::future::Future;
use std::task::{Poll, Waker};
use futures_util::future::LocalFutureObj;
use log::info;

fn set_logger() {
    #[cfg(target_arch = "wasm32")]
    web_logger::custom_init(web_logger::Config {
        level: log::Level::Debug,
    });
    #[cfg(not(target_arch = "wasm32"))]
    simple_logger::init_with_level(log::Level::Debug).expect("A logger was already initialized");
}

fn main() {
    set_logger();
    run(Settings::default(), app);
}

#[derive(Debug)]
enum CustomEvent {
    Ticked
}

#[derive(Debug)]
enum LocalEvent {
    Blinds(BlindsEvent),
    Custom(CustomEvent),
    TaskFinished
}

/**
 * Bugs!
 * 
 * #1 Without a real sleep/timer/alarm, the tick_loop is "uncooperative" and doesn't suspend
 * #2 Without any tasks running, polling the FuturedUnordered is always Ready. Need to use a waker.
 */

#[cfg(not(target_arch = "wasm32"))]
mod sleep {
    use async_std::task::sleep;
    use std::time::Duration;
    
    pub async fn sleep_1() { sleep(Duration::from_secs(1)).await }
}

#[cfg(all(feature = "stdweb", target_arch = "wasm32"))]
mod sleep {
    extern crate std_web;

    use std_web::web::wait;

    pub async fn sleep_1() { wait(1000).await }
}

#[cfg(all(feature = "web-sys", target_arch = "wasm32"))]
mod sleep {
    extern crate js_sys;
    extern crate wasm_bindgen_futures;

    use std::future::Future;
    use futures_util::future::pending;
    use web_sys::window;
    use js_sys::{Function, Promise, Array};
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen::closure::Closure;
    use wasm_bindgen::JsValue;
    use wasm_bindgen::JsCast;
    use log::info;

    /**
     * This fails at runtime. I don't think things are compatible...
     */

    pub async fn sleep_1() {
        info!("TICK");
        let cb = Closure::wrap(Box::new(|| {
            info!("TOCK")
        }) as Box<dyn FnMut()>);
        let window = window().expect("Failed to get window");
        window.set_timeout_with_callback_and_timeout_and_arguments(cb.as_ref().unchecked_ref(), 1000, &Array::new()).expect("Set timeout");

        // JsFuture::from(Promise::new(|resolve: Function, reject: Function| {
        //     let cb = Closure::wrap(Box::new(|| {
        //         resolve.call0(&JsValue::NULL);
        //     }) as Box<FnMut()>);
        //     let cbf: Function = cb.as_ref().unchecked_ref();
        //     window.set_timeout_with_callback(&cbf);
        // }));
    }
}

async fn tick_loop(local_events: Arc<RefCell<MyEventBuffer<CustomEvent>>>)  {
    loop {
        local_events.borrow_mut().push(CustomEvent::Ticked);
        sleep::sleep_1().await
    }
}

async fn next_event(mut event_stream: EventStream) -> LocalEvent {
    LocalEvent::Blinds(event_stream.next_event_blocking().await)
}

async fn next_finished_task<Fut>(futures_cell: Arc<RefCell<FuturesUnordered<Fut>>>) -> LocalEvent where Fut: Future<Output = ()>  {
    let futures_cell = futures_cell.clone();
    poll_fn(move |cx| {
        let mut x = futures_cell.borrow_mut();
        let pinned_pool = Pin::new(&mut *x);
        let poll_result = pinned_pool.poll_next(cx);
        poll_result
    }).await;
    LocalEvent::TaskFinished
}

async fn app(_window: Window, event_stream: EventStream) {
    // Setup all of the crap
    let mut local_events: MyEventStream<CustomEvent> = MyEventStream::new();
    let futures_pool: FuturesUnordered<LocalFutureObj<()>> = FuturesUnordered::new();
    let futures_cell = Arc::new(RefCell::new(futures_pool));

    // Spawn a never-ending task to keep the future pool from spinning freely. Hack for bug #2
    futures_cell.borrow().spawn_local(pending()).expect("Failed to start pending task");

    // Spawn the loop task
    // !!! Disabled because it needs to go into Pending somehow !!!
    futures_cell.borrow().spawn_local(tick_loop(local_events.buffer())).expect("Failed to start tick loop");

    'main: loop {
        // Define all of the possible futures (now all same type)
        let task = next_finished_task(futures_cell.clone()).boxed_local();
        let blinds = next_event(event_stream.clone()).boxed_local();
        let local = local_events.next_event_blocking().map(|ev| LocalEvent::Custom(ev)).boxed_local();

        // Wait for the first one
        let (ev, _index, _remaining) = select_all(vec!(task, blinds, local)).await;

        // Switch
        match ev {
            LocalEvent::Blinds(ev) => info!("Blinds Event {:?}", ev),
            LocalEvent::Custom(ev) => info!("Custom Event {:?}", ev),
            LocalEvent::TaskFinished => info!("Task finished")
        }
    }
}

/**
 * Everything below here is a copy of something already in the repository, but made generic
 */

// #[derive(Clone)]
pub struct MyEventStream<E> {
    buffer: Arc<RefCell<MyEventBuffer<E>>>,
}

impl <E> MyEventStream<E> {
    pub(crate) fn new() -> Self {
        MyEventStream {
            buffer: Arc::new(RefCell::new(MyEventBuffer {
                events: VecDeque::new(),
                waker: None,
                ready: false,
            })),
        }
    }

    pub(crate) fn buffer(&self) -> Arc<RefCell<MyEventBuffer<E>>> {
        self.buffer.clone()
    }

    // FIXME: change the type 
    pub fn next_event_blocking<'a>(&'a mut self) -> impl 'a + Future<Output = E> {
        poll_fn(move |cx| {
            let mut buffer = self.buffer.borrow_mut();
            match buffer.events.pop_front() {
                Some(event) => Poll::Ready(event),
                None => {
                    if buffer.ready {
                        buffer.ready = false
                    }
                    buffer.waker = Some(cx.waker().clone());
                    Poll::Pending
                }
            }
        })
    }
}

// #[derive(Clone)]
pub(crate) struct MyEventBuffer<E> {
    events: VecDeque<E>,
    waker: Option<Waker>,
    ready: bool,
}

impl <E> MyEventBuffer<E> {
    pub fn push(&mut self, event: E) {
        self.events.push_back(event);
        self.mark_ready();
    }

    pub fn mark_ready(&mut self) {
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
        self.ready = true;
    }
}
