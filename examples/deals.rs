extern crate rand;
extern crate getopts;
extern crate timely;
extern crate timely_sort;
extern crate graph_map;
extern crate differential_dataflow;
extern crate vec_map;

use std::time::Instant;
use std::hash::Hash;
use std::mem;

use vec_map::VecMap;

use rand::{Rng, SeedableRng, StdRng};

use timely_sort::Unsigned;

use timely::dataflow::*;
use timely::dataflow::operators::*;

use differential_dataflow::Collection;
use differential_dataflow::operators::*;
use differential_dataflow::operators::join::JoinArranged;
use differential_dataflow::lattice::Lattice;

use graph_map::GraphMMap;

type Node = u32;
type Edge = (Node, Node);

fn main() {

    // snag a filename to use for the input graph.
    let filename = std::env::args().nth(1).unwrap();
    let program = std::env::args().nth(2).unwrap(); 

    timely::execute_from_args(std::env::args().skip(1), move |computation| {

        let peers = computation.peers();
        let index = computation.index();

        // // What you might do if you used GraphMMap:
        let graph = GraphMMap::new(&filename);

        let (mut input, mut query, probe) = computation.scoped::<u64,_,_>(|scope| {

            let (input, stream1) = scope.new_input();
            let (rootz, stream2) = scope.new_input();

            let graph = Collection::new(stream1);
            let query = Collection::new(stream2);

            let probe = match program.as_str() {
                "reach" => _reach(&graph, &query).inner.probe().0,
                "cc"    => _connected_components(&graph).inner.probe().0,
                "bfs"   => _bfs(&graph, &query).inner.probe().0,
                "pymk"  => _pymk(&graph, &query, 10).inner.probe().0,
                _       => panic!("must specify one of 'reach', 'cc', 'bfs'.")
            };

            (input, rootz, probe)
        });
        

        let timer = Instant::now();

        // start loading up the graph
        for node in 0..graph.nodes() {
            if node % peers == index {
                for &edge in graph.edges(node) {
                    input.send(((node as u32, edge), 1));
                }
            }
        }

        // run until graph is loaded
        input.advance_to(1);
        query.advance_to(1);
        computation.step_while(|| probe.lt(query.time()));

        if index == 0 {
            println!("loaded: {:?}", timer.elapsed());
        }

        // conduct latencies.capacity() measurements.
        let mut latencies = Vec::with_capacity(11);

        let seed: &[_] = &[1, 2, 3, 4];
        let mut rng: StdRng = SeedableRng::from_seed(seed);    // rng for edge additions

        for _count in 0..latencies.capacity() {
            let timer = Instant::now();
            if index == 0 {
                query.send((rng.gen_range(0, graph.nodes() as u32), 1));
            }
            let next = query.epoch() + 1;
            input.advance_to(next);
            query.advance_to(next);
            while probe.lt(query.time()) { computation.step(); }
            latencies.push(timer.elapsed());
        }

        if index == 0 {
            for lat in &latencies {
                println!("latency: {:?}", lat);
            }
        }

    }).unwrap();
}

// returns pairs (root, friend-of-friend) for the top-k friends of friends by count.
fn _pymk<G: Scope>(edges: &Collection<G, Edge>, query: &Collection<G, Node>, k: usize) -> Collection<G, (Node,Node)>
where G::Timestamp: Lattice {

    // symmetrize the graph
    let edges = edges.map_in_place(|x: &mut (u32, u32)| ::std::mem::swap(&mut x.0, &mut x.1))
                     .concat(&edges);

    // "arrange" edges, because we'll want to use it twice the same way.
    let edges = edges.arrange_by_key(|k| k.clone(), |x| (VecMap::new(), x));
    let query = query.arrange_by_self(|k: &u32| k.as_u64(), |x| (VecMap::new(), x));

    // restrict attention to edges from query nodes
    edges.join(&query, |k,v,_| (v.clone(), k.clone()))
         .arrange_by_key(|k| k.clone(), |x| (VecMap::new(), x))
         .join(&edges, |_,x,y| (x.clone(), y.clone()))
         // the next thing is the "topk" computation. sorry!
         .group_u(move |_,s,t| {
             t.extend(s.map(|(x,y)| (*x,y)));       // propose all inputs as outputs
             t.sort_by(|x,y| (-x.1).cmp(&(-y.1)));  // sort by negative count (large numbers first)
             t.truncate(k)                          // keep at most k of these
         })
 }

// returns pairs (n, s) indicating node n can be reached from a root in s steps.
fn _reach<G: Scope>(edges: &Collection<G, Edge>, query: &Collection<G, Node>) -> Collection<G, Node>
where G::Timestamp: Lattice {

    // initialize query as reaching themselves at distance 0
    // repeatedly update minimal distances each node can be reached from each root
    query.iterate(|inner| {

        let edges = edges.enter(&inner.scope());
        let nodes = query.enter(&inner.scope());

        // edges from active sources activate their destinations
        edges.semijoin_u(&inner)
             .map(|(_,d)| d)
             .concat(&nodes)
             .distinct_u()
     })
}


// returns pairs (node, label) indicating the connected component containing each node
fn _connected_components<G: Scope>(edges: &Collection<G, Edge>) -> Collection<G, (Node, Node)>
where G::Timestamp: Lattice+Hash {

    // each edge (x,y) means that we need at least a label for the min of x and y.
    let nodes = edges.map_in_place(|pair| {
                        let min = std::cmp::min(pair.0, pair.1);
                        *pair = (min, min);
                     })
                     .consolidate_by(|x| x.0);

    // each edge should exist in both directions.
    let edges = edges.map_in_place(|x| mem::swap(&mut x.0, &mut x.1))
                     .concat(&edges);

    // don't actually use these labels, just grab the type
    nodes.filter(|_| false)
         .iterate(|inner| {
             let edges = edges.enter(&inner.scope());
             let nodes = nodes.enter_at(&inner.scope(), |r| 256 * (64 - (r.0).0.leading_zeros() as u64));

             inner.join_map_u(&edges, |_k,l,d| (*d,*l))
                  .concat(&nodes)
                  .group_u(|_, mut s, t| { t.push((*s.peek().unwrap().0, 1)); } )
         })
}

// returns pairs (n, s) indicating node n can be reached from a root in s steps.
fn _bfs<G: Scope>(edges: &Collection<G, Edge>, query: &Collection<G, Node>) -> Collection<G, (Node, u32)>
where G::Timestamp: Lattice {

    // initialize query as reaching themselves at distance 0
    let nodes = query.map(|x| (x, 0));

    // repeatedly update minimal distances each node can be reached from each root
    nodes.iterate(|inner| {

        let edges = edges.enter(&inner.scope());
        let nodes = nodes.enter(&inner.scope());

        inner.join_map_u(&edges, |_k,l,d| (*d, l+1))
             .concat(&nodes)
             .group_u(|_, s, t| t.push((*s.peek().unwrap().0, 1)))
     })
}