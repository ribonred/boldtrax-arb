use std::future::Future;
use tokio::sync::mpsc;

/// Spawns an asynchronous task and sends its result back through an MPSC channel.
/// This is ideal for actor patterns where you want to offload IO work (like REST calls)
/// and receive the result as a message in the actor's main event loop.
pub fn spawn_and_send<T, Fut, Msg, F>(tx: mpsc::Sender<Msg>, future: Fut, map_result: F)
where
    Fut: Future<Output = T> + Send + 'static,
    Msg: Send + 'static,
    F: FnOnce(T) -> Msg + Send + 'static,
{
    tokio::spawn(async move {
        let result = future.await;
        let msg = map_result(result);
        // If the receiver is dropped, the actor is dead, so we can safely ignore the send error
        let _ = tx.send(msg).await;
    });
}
