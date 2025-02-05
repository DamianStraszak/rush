use crate::{
    nodes::{NodeCount, NodeIndex, NodeMap},
    Config, HashT, NodeIdT, NotificationOut, PreUnit, Receiver, Round, Sender, Unit,
};
use futures::{FutureExt, StreamExt};
use log::{debug, error};
use tokio::{
    sync::oneshot,
    time::{delay_for, Duration},
};

/// A process responsible for creating new units. It receives all the units added locally to the Dag
/// via the parents_rx channel endpoint. It creates units according to an internal strategy respecting
/// always the following constraints: for a unit U of round r
/// - all U's parents are from round (r-1),
/// - all U's parents are created by different nodes,
/// - one of U's parents is the (r-1)-round unit by U's creator,
/// - U has > floor(2*N/3) parents.
/// The currently implemented strategy creates the unit U at the very first moment when enough
/// candidates for parents are available for all the above constraints to be satisfied.
pub(crate) struct Creator<H: HashT, NI: NodeIdT> {
    node_id: NI,
    parents_rx: Receiver<Unit<H>>,
    new_units_tx: Sender<NotificationOut<H>>,
    n_members: NodeCount,
    current_round: Round, // current_round is the round number of our next unit
    candidates_by_round: Vec<NodeMap<Option<H>>>,
    n_candidates_by_round: Vec<NodeCount>,
    hashing: Box<dyn Fn(&[u8]) -> H + Send>,
    create_lag: Duration,
}

impl<H: HashT, NI: NodeIdT> Creator<H, NI> {
    pub(crate) fn new(
        conf: Config<NI>,
        parents_rx: Receiver<Unit<H>>,
        new_units_tx: Sender<NotificationOut<H>>,
        hashing: impl Fn(&[u8]) -> H + Send + 'static,
    ) -> Self {
        let Config {
            node_id,
            n_members,
            create_lag,
        } = conf;
        Creator {
            node_id,
            parents_rx,
            new_units_tx,
            n_members,
            current_round: 0,
            candidates_by_round: vec![NodeMap::new_with_len(n_members)],
            n_candidates_by_round: vec![NodeCount(0)],
            hashing: Box::new(hashing),
            create_lag,
        }
    }

    // initializes the vectors corresponding to the given round (and all between if not there)
    fn init_round(&mut self, round: Round) {
        while self.candidates_by_round.len() <= round {
            self.candidates_by_round
                .push(NodeMap::new_with_len(self.n_members));
            self.n_candidates_by_round.push(NodeCount(0));
        }
    }

    fn create_unit(&mut self) {
        let round = self.current_round;
        let parents = {
            if round == 0 {
                NodeMap::new_with_len(self.n_members)
            } else {
                self.candidates_by_round[round - 1].clone()
            }
        };

        let new_preunit = PreUnit::new_from_parents(
            self.node_id.my_index().unwrap(),
            round,
            parents,
            &self.hashing,
        );
        debug!(target: "rush-creator", "{} Created a new unit {:?} at round {}.", self.node_id, new_preunit, self.current_round);
        let send_result = self.new_units_tx.send(new_preunit.into());
        if let Err(e) = send_result {
            error!(target: "rush-creator", "{:?} Unable to send a newly created unit: {:?}.", self.node_id, e);
        }

        self.current_round += 1;
        self.init_round(self.current_round);
    }

    fn add_unit(&mut self, round: Round, pid: NodeIndex, hash: H) {
        // units that are too old are of no interest to us
        if round + 1 >= self.current_round {
            self.init_round(round);
            if self.candidates_by_round[round][pid].is_none() {
                // passing the check above means that we do not have any unit for the pair (round, pid) yet
                self.candidates_by_round[round][pid] = Some(hash);
                self.n_candidates_by_round[round] += NodeCount(1);
            }
        }
    }

    fn check_ready(&self) -> bool {
        if self.current_round == 0 {
            return true;
        }
        // To create a new unit, we need to have at least >floor(2*N/3) parents available in previous round.
        // Additionally, our unit from previous round must be available.
        let prev_round = self.current_round - 1;
        let threshold = (self.n_members * 2) / 3;

        self.n_candidates_by_round[prev_round] > threshold
            && self.candidates_by_round[prev_round][self.node_id.my_index().unwrap()].is_some()
    }

    pub(crate) async fn create(&mut self, exit: oneshot::Receiver<()>) {
        self.create_unit();
        let mut exit = exit.into_stream();
        loop {
            tokio::select! {
                Some(u) = self.parents_rx.recv() => {
                    self.add_unit(u.round(), u.creator(), u.hash());
                    if self.check_ready() {
                        self.create_unit();
                        delay_for(self.create_lag).await;
                    }
                }
                _ = exit.next() => {
                    debug!(target: "rush-creator", "{} received exit signal.", self.node_id);
                    break
                }
            }
        }
    }
}
