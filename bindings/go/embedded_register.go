//go:build kevy_embedded

package kevy

// Installing the seam is the tagged build's only job here: everything
// else about the embedded backend lives in kevy.go and emb_sub.go. Keep
// it in its own file so the registration is impossible to miss when
// reading either side.
func init() {
	openEmbeddedStore = func(t target) (embStore, error) {
		var (
			d   *DB
			err error
		)
		switch t.kind {
		case targetMemAnon, targetMemNamed:
			d, err = OpenMem()
		case targetFile:
			d, err = Open(t.path)
		default:
			return nil, errInvalidInput("openEmbeddedStore on a non-embedded target")
		}
		if err != nil {
			return nil, err
		}
		return embDB{d}, nil
	}
	newEmbSubscriber = func(db embStore, key string) embSubscriber {
		return &embSub{db: db, key: key}
	}
}

// embDB adapts *DB to the seam. Only one method needs it: subBytes
// returns the concrete *Sub, and Go does not treat a method returning a
// concrete type as satisfying one that returns an interface. Adapting
// here keeps *DB's own signature — which is public API — unchanged.
type embDB struct{ *DB }

func (e embDB) subBytes(name []byte, pattern bool) (embSubscription, error) {
	s, err := e.DB.subBytes(name, pattern)
	if err != nil {
		return nil, err
	}
	return s, nil
}
