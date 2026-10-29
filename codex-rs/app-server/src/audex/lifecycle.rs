use codex_extension_api::{
    ExtensionData, ExtensionFuture, TurnAbortInput, TurnErrorInput, TurnLifecycleContributor,
    TurnStartInput, TurnStopInput,
};
use codex_protocol::items::TurnItem;
use std::sync::Arc;

/// Hints only. Durable history remains the evidence used by later workflow recovery.
#[derive(Clone, Debug)]
pub(super) enum ReviewerObservation {
    ItemCompleted {
        assignment_id: String,
        turn_id: String,
        item: Box<TurnItem>,
    },
    Stopped {
        assignment_id: String,
        turn_id: String,
    },
    Aborted {
        assignment_id: String,
        turn_id: String,
    },
    Error {
        assignment_id: String,
        turn_id: String,
    },
}

/// The host supplies a prompt, non-blocking observer; this contributor never consumes next_event().
pub(super) struct ReviewerLifecycle {
    pub assignment_id: String,
    pub observe: Arc<dyn Fn(ReviewerObservation) + Send + Sync>,
}

struct ReviewTurnId(String);

impl ReviewerLifecycle {
    fn turn_id(store: &ExtensionData) -> String {
        store
            .get::<ReviewTurnId>()
            .map(|id| id.0.clone())
            .unwrap_or_else(|| store.level_id().to_string())
    }
}

impl TurnLifecycleContributor for ReviewerLifecycle {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            input
                .turn_store
                .insert(ReviewTurnId(input.turn_id.to_string()));
        })
    }
    fn on_item_completed<'a>(
        &'a self,
        _: &'a ExtensionData,
        turn: &'a ExtensionData,
        item: &'a TurnItem,
    ) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            (self.observe)(ReviewerObservation::ItemCompleted {
                assignment_id: self.assignment_id.clone(),
                turn_id: Self::turn_id(turn),
                item: Box::new(item.clone()),
            });
        })
    }
    fn on_turn_stop<'a>(&'a self, input: TurnStopInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            (self.observe)(ReviewerObservation::Stopped {
                assignment_id: self.assignment_id.clone(),
                turn_id: Self::turn_id(input.turn_store),
            });
        })
    }
    fn on_turn_abort<'a>(&'a self, input: TurnAbortInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            (self.observe)(ReviewerObservation::Aborted {
                assignment_id: self.assignment_id.clone(),
                turn_id: Self::turn_id(input.turn_store),
            });
        })
    }
    fn on_turn_error<'a>(&'a self, input: TurnErrorInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            (self.observe)(ReviewerObservation::Error {
                assignment_id: self.assignment_id.clone(),
                turn_id: input.turn_id.to_string(),
            });
        })
    }
}
