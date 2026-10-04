use tokio::sync::watch;

pub type Receiver = watch::Receiver<Option<Result<(), String>>>;

pub fn listen() -> Receiver {
    let (sender, receiver) = watch::channel(None);
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let signals = signal(SignalKind::interrupt()).and_then(|interrupt| {
            signal(SignalKind::terminate()).map(|terminate| (interrupt, terminate))
        });
        match signals {
            Ok((mut interrupt, mut terminate)) => {
                tokio::spawn(async move {
                    tokio::select! {
                        _ = interrupt.recv() => {}
                        _ = terminate.recv() => {}
                    }
                    sender.send_replace(Some(Ok(())));
                });
            }
            Err(error) => {
                sender.send_replace(Some(Err(error.to_string())));
            }
        }
    }
    #[cfg(not(unix))]
    tokio::spawn(async move {
        let result = tokio::signal::ctrl_c().await.map_err(|error| error.to_string());
        sender.send_replace(Some(result));
    });
    receiver
}

pub async fn requested(receiver: &mut Receiver) -> Result<(), String> {
    loop {
        let current = receiver.borrow().clone();
        if let Some(result) = current {
            return result;
        }
        receiver
            .changed()
            .await
            .map_err(|_| "shutdown signal listener stopped unexpectedly".to_owned())?;
    }
}
