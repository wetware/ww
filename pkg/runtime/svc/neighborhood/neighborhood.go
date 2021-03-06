package neighborhood

import (
	"context"
	"fmt"

	"github.com/libp2p/go-eventbus"
	"github.com/libp2p/go-libp2p-core/event"
	"github.com/libp2p/go-libp2p-core/network"
	"github.com/libp2p/go-libp2p-core/peer"
	ww "github.com/wetware/ww/pkg"
	"github.com/wetware/ww/pkg/runtime"
	"github.com/wetware/ww/pkg/runtime/svc/internal"
	"go.uber.org/fx"
	"go.uber.org/multierr"
)

// Config for Neighborhood service
type Config struct {
	fx.In

	Bus  event.Bus
	KMin int `name:"kmin"`
	KMax int `name:"kmax"`
}

// NewService satisfies runtime.ServiceFactory
func (cfg Config) NewService() (runtime.Service, error) {
	sub, err := cfg.Bus.Subscribe(new(event.EvtPeerConnectednessChanged))
	if err != nil {
		return nil, err
	}

	e, err := cfg.Bus.Emitter(new(EvtNeighborhoodChanged), eventbus.Stateful)
	if err != nil {
		return nil, err
	}

	return neighborhood{
		phaseMap: phasemap(cfg.KMin, cfg.KMax),
		bus:      cfg.Bus,
		sub:      sub,
		e:        e,
		cq:       make(chan struct{}),
	}, nil
}

// Produces EvtNeighborhoodChanged.
func (cfg Config) Produces() []interface{} {
	return []interface{}{
		EvtNeighborhoodChanged{},
	}
}

// Consumes event.EvtPeerConnectednessChanged.
func (cfg Config) Consumes() []interface{} {
	return []interface{}{
		event.EvtPeerConnectednessChanged{}, // see comment in tracker service
	}
}

// Module for Neighborhood service
type Module struct {
	fx.Out

	Factory runtime.ServiceFactory `group:"runtime"`
}

// EvtNeighborhoodChanged fires when a graph edge is created or destroyed
type EvtNeighborhoodChanged struct {
	K        int
	From, To Phase
}

// New Neighborhood service.  Maintains graph connectivity.
//
// Consumes:
//  - p2p.EvtNetworkReady
// 	- event.EvtPeerConnectednessChanged [ libp2p ]
//
// Emits:
//	- EvtNeighborhoodChanged
func New(cfg Config) Module { return Module{Factory: cfg} }

// neighborhood notifies subscribers of changes in direct connectivity to remote
// hosts.  Neighborhood events do not concern themselves with the number of connections,
// but rather the presence or absence of a direct link.
type neighborhood struct {
	log ww.Logger
	phaseMap

	bus event.Bus
	sub event.Subscription
	e   event.Emitter
	cq  chan struct{}
}

func (n neighborhood) Loggable() map[string]interface{} {
	return map[string]interface{}{"service": "neighborhood"}
}

func (n neighborhood) Start(ctx context.Context) (err error) {
	if err = internal.WaitNetworkReady(ctx, n.bus); err == nil {
		internal.StartBackground(n.subloop)

		// signal initial state - PhaseOrphaned
		err = n.e.Emit(EvtNeighborhoodChanged{})
	}

	return
}

func (n neighborhood) Stop(context.Context) error {
	close(n.cq)

	return multierr.Combine(
		n.sub.Close(),
		n.e.Close(),
	)
}

func (n neighborhood) subloop() {
	var state EvtNeighborhoodChanged
	var ps = make(map[peer.ID]struct{})

	for v := range n.sub.Out() {
		switch ev := v.(event.EvtPeerConnectednessChanged); ev.Connectedness {
		case network.Connected:
			ps[ev.Peer] = struct{}{}
		case network.NotConnected:
			delete(ps, ev.Peer)
		default:
			panic("Unreachable ... unless libp2p has fixed event.PeerConnectednessChanged!!")
		}

		state.K = len(ps)
		state.From = state.To
		state.To = n.Phase(len(ps))

		if err := n.e.Emit(state); err != nil {
			n.log.With(n).WithError(err).Error("failed to emit EvtNeighborhoodChanged")
		}
	}
}

// Phase is the codomain in the function ƒ: C ⟼ P,
// where C ∈ ℕ and P ∈ {orphaned, partial, complete, overloaded}.  Members of P are
// defined as follows:
//
// Let k ∈ C be the number of remote hosts to which we are connected, and let l, h ∈ ℕ
// be the low-water and high-water marks, respectively.
//
// Then:
// - orphaned := k == 0
// - partial := 0 < k < l
// - complete := l <= k <= h
// - overloaded := k > h
type Phase uint8

const (
	// PhaseOrphaned indicates the Host is not connected to the graph.
	PhaseOrphaned Phase = iota
	// PhasePartial indicates the Host is weakly connected to the graph.
	PhasePartial
	// PhaseComplete indicates the Host is strongly connected to the graph.
	PhaseComplete
	// PhaseOverloaded indicates the Host is strongly connected to the graph, but
	// should have its connections pruned to reduce resource consumption.
	PhaseOverloaded
)

func (p Phase) String() string {
	switch p {
	case PhaseOrphaned:
		return "host orphaned"
	case PhasePartial:
		return "neighborhood partial"
	case PhaseComplete:
		return "neighborhood complete"
	case PhaseOverloaded:
		return "neighborhood overloaded"
	default:
		return fmt.Sprintf("<invalid phase:: %d>", p)
	}
}

type phaseMap struct {
	l, h int
}

func phasemap(l, h int) phaseMap {
	return phaseMap{l: l, h: h}
}

func (p phaseMap) Phase(k int) Phase {
	switch {
	case k == 0:
		return PhaseOrphaned
	case 0 < k && k < p.l:
		return PhasePartial
	case p.l <= k && k <= p.h:
		return PhaseComplete
	case k > p.h:
		return PhaseOverloaded
	default:
		panic(fmt.Sprintf("invalid cardinality:  %d", k))
	}
}
