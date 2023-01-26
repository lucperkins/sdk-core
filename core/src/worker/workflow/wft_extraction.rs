use crate::{
    abstractions::OwnedMeteredSemPermit,
    protosext::ValidPollWFTQResponse,
    worker::{
        client::WorkerClient,
        workflow::{
            history_update::HistoryPaginator, CacheMissFetchReq, HistoryUpdate, NextPageReq,
            PermittedWFT,
        },
    },
};
use futures::Stream;
use futures_util::{stream, stream::PollNext, FutureExt, StreamExt};
use std::{future, sync::Arc};
use tracing::Span;

/// Transforms incoming validated WFTs and history fetching requests into [PermittedWFT]s ready
/// for application to workflow state
pub(super) struct WFTExtractor {}

pub(super) enum WFTExtractorOutput {
    NewWFT(PermittedWFT),
    FetchResult(PermittedWFT, Arc<HistfetchRC>),
    NextPage {
        paginator: HistoryPaginator,
        update: HistoryUpdate,
        span: Span,
        rc: Arc<HistfetchRC>,
    },
    FailedFetch {
        run_id: String,
        err: tonic::Status,
    },
    PollerDead,
}

type WFTStreamIn = (
    Result<ValidPollWFTQResponse, tonic::Status>,
    OwnedMeteredSemPermit,
);
#[derive(derive_more::From, Debug)]
pub(super) enum HistoryFetchReq {
    Full(CacheMissFetchReq, Arc<HistfetchRC>),
    NextPage(NextPageReq, Arc<HistfetchRC>),
}
/// Used inside of `Arc`s to ensure we don't shutdown while there are outstanding fetches.
#[derive(Debug)]
pub(super) struct HistfetchRC {}

impl WFTExtractor {
    pub(super) fn build(
        client: Arc<dyn WorkerClient>,
        max_fetch_concurrency: usize,
        wft_stream: impl Stream<Item = WFTStreamIn> + Send + 'static,
        fetch_stream: impl Stream<Item = HistoryFetchReq> + Send + 'static,
    ) -> impl Stream<Item = Result<WFTExtractorOutput, tonic::Status>> + Send + 'static {
        let fetch_client = client.clone();
        let wft_stream = wft_stream
            .map(move |(wft, permit)| {
                let client = client.clone();
                async move {
                    match wft {
                        Ok(wft) => {
                            let prev_id = wft.previous_started_event_id;
                            let run_id = wft.workflow_execution.run_id.clone();
                            Ok(
                                match HistoryPaginator::from_poll(wft, client, prev_id).await {
                                    Ok((pag, prep)) => WFTExtractorOutput::NewWFT(PermittedWFT {
                                        work: prep,
                                        permit,
                                        paginator: pag,
                                    }),
                                    Err(err) => WFTExtractorOutput::FailedFetch { run_id, err },
                                },
                            )
                        }
                        Err(e) => Err(e),
                    }
                }
                // This is... unattractive, but lets us avoid boxing all the futs in the stream
                .left_future()
                .left_future()
            })
            .chain(stream::iter([future::ready(Ok(
                WFTExtractorOutput::PollerDead,
            ))
            .right_future()
            .left_future()]));

        stream::select_with_strategy(
            wft_stream,
            fetch_stream.map(move |fetchreq: HistoryFetchReq| {
                let client = fetch_client.clone();
                async move {
                    Ok(match fetchreq {
                        // It's OK to simply drop the refcounters in the event of fetch
                        // failure. We'll just proceed with shutdown.
                        HistoryFetchReq::Full(req, rc) => {
                            let run_id = req.original_wft.work.execution.run_id.clone();
                            match HistoryPaginator::from_fetchreq(req, client).await {
                                Ok(r) => WFTExtractorOutput::FetchResult(r, rc),
                                Err(err) => WFTExtractorOutput::FailedFetch { run_id, err },
                            }
                        }
                        HistoryFetchReq::NextPage(mut req, rc) => {
                            match req
                                .paginator
                                .extract_next_update(req.last_processed_id)
                                .await
                            {
                                Ok(update) => WFTExtractorOutput::NextPage {
                                    paginator: req.paginator,
                                    update,
                                    span: req.span,
                                    rc,
                                },
                                Err(err) => WFTExtractorOutput::FailedFetch {
                                    run_id: req.paginator.run_id,
                                    err,
                                },
                            }
                        }
                    })
                }
                .right_future()
            }),
            // Priority always goes to the fetching stream
            |_: &mut ()| PollNext::Right,
        )
        .buffer_unordered(max_fetch_concurrency)
    }
}