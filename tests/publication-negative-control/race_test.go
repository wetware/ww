package publicationnegativecontrol

import (
	"context"
	"crypto/rand"
	"fmt"
	"os"
	"sync/atomic"
	"testing"
	"time"

	"github.com/ipfs/boxo/ipns"
	"github.com/ipfs/boxo/path"
	"github.com/ipfs/boxo/routing/offline"
	"github.com/ipfs/go-cid"
	ds "github.com/ipfs/go-datastore"
	leveldb "github.com/ipfs/go-ds-leveldb"
	record "github.com/libp2p/go-libp2p-record"
	ic "github.com/libp2p/go-libp2p/core/crypto"
	"github.com/libp2p/go-libp2p/core/peer"
	"github.com/multiformats/go-multihash"
)

// barrierStore delays one already-entered datastore Put. It does not edit the
// upstream OfflineRouter. The barrier is after OfflineRouter's validation and
// older-record selection and before the actual datastore mutation. Datastore
// cancellation does not prove that an already-entered write has stopped.
type barrierStore struct {
	ds.Datastore
	armed   atomic.Bool
	entered chan struct{}
	release chan struct{}
}

func (s *barrierStore) Put(ctx context.Context, key ds.Key, value []byte) error {
	if s.armed.CompareAndSwap(true, false) {
		close(s.entered)
		<-s.release
	}
	return s.Datastore.Put(ctx, key, value)
}

type fixture struct {
	store   *barrierStore
	put     func(context.Context, []byte) error
	get     func() uint64
	records [3][]byte
}

func newFixture(t *testing.T) fixture {
	t.Helper()
	key, _, err := ic.GenerateEd25519Key(rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	peerID, err := peer.IDFromPrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	name := ipns.NameFromPeer(peerID)
	diskStore, err := leveldb.NewDatastore(t.TempDir(), nil)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := diskStore.Close(); err != nil {
			t.Error(err)
		}
	})
	store := &barrierStore{
		Datastore: diskStore,
		entered:   make(chan struct{}), release: make(chan struct{}),
	}
	router := offline.NewOfflineRouter(store, record.NamespacedValidator{
		"ipns": ipns.Validator{},
	})
	f := fixture{store: store}
	f.put = func(ctx context.Context, raw []byte) error {
		return router.PutValue(ctx, string(name.RoutingKey()), raw)
	}
	f.get = func() uint64 {
		raw, err := router.GetValue(context.Background(), string(name.RoutingKey()))
		if err != nil {
			t.Fatal(err)
		}
		rec, err := ipns.UnmarshalRecord(raw)
		if err != nil {
			t.Fatal(err)
		}
		if err := ipns.ValidateWithName(rec, name); err != nil {
			t.Fatal(err)
		}
		sequence, err := rec.Sequence()
		if err != nil {
			t.Fatal(err)
		}
		return sequence
	}
	for sequence := range f.records {
		digest, err := multihash.Sum([]byte(fmt.Sprintf("release-%d", sequence)), multihash.SHA2_256, -1)
		if err != nil {
			t.Fatal(err)
		}
		value, err := path.NewPath("/ipfs/" + cid.NewCidV1(cid.Raw, digest).String())
		if err != nil {
			t.Fatal(err)
		}
		rec, err := ipns.NewRecord(key, value, uint64(sequence), time.Now().Add(time.Hour), time.Minute)
		if err != nil {
			t.Fatal(err)
		}
		f.records[sequence], err = ipns.MarshalRecord(rec)
		if err != nil {
			t.Fatal(err)
		}
		if err := ipns.ValidateWithName(rec, name); err != nil {
			t.Fatal(err)
		}
	}
	if err := f.put(context.Background(), f.records[0]); err != nil {
		t.Fatal(err)
	}
	return f
}

func TestSequentialOlderRecordIsRejected(t *testing.T) {
	f := newFixture(t)
	if err := f.put(context.Background(), f.records[2]); err != nil {
		t.Fatal(err)
	}
	if err := f.put(context.Background(), f.records[1]); err == nil {
		t.Fatal("sequential older record unexpectedly accepted")
	} else {
		t.Logf("normal sequential stale PUT refused: %v", err)
	}
	if sequence := f.get(); sequence != 2 {
		t.Fatalf("want sequence 2; got %d", sequence)
	}
}

func TestDelayedOlderRecordCanOverwriteNewer(t *testing.T) {
	f := newFixture(t)
	f.store.armed.Store(true)
	oldContext, cancelOld := context.WithCancel(context.Background())
	defer cancelOld()
	delayedDone := make(chan error, 1)
	go func() { delayedDone <- f.put(oldContext, f.records[1]) }()
	select {
	case <-f.store.entered:
	case <-time.After(5 * time.Second):
		t.Fatal("old PUT never reached post-selection datastore barrier")
	}
	// Model the publication waiter going away while the accepted backend write
	// remains in flight. The state machine then retries the exact signed bytes.
	cancelOld()
	if err := f.put(context.Background(), f.records[1]); err != nil {
		t.Fatal(err)
	}
	if sequence := f.get(); sequence != 1 {
		t.Fatalf("retry: want sequence 1; got %d", sequence)
	}
	if err := f.put(context.Background(), f.records[2]); err != nil {
		t.Fatal(err)
	}
	if sequence := f.get(); sequence != 2 {
		t.Fatalf("new publication: want sequence 2; got %d", sequence)
	}
	t.Log("same-intent retry completed, then serialized newer publication acknowledged sequence 2")
	close(f.store.release)
	select {
	case err := <-delayedDone:
		if err != nil {
			t.Fatalf("delayed old PUT: %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("delayed PUT never finished")
	}
	sequence := f.get()
	t.Logf("after delayed old datastore write: authenticated GET sequence=%d", sequence)
	if os.Getenv("WW_REQUIRE_MONOTONIC") == "1" {
		if sequence != 2 {
			t.Fatalf("Publisher contract violated: sequence regressed from 2 to %d", sequence)
		}
	} else if sequence != 1 {
		t.Fatalf("negative control did not reproduce pinned race: want sequence 1; got %d", sequence)
	}
}
