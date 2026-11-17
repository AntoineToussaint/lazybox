package engine

// turnStop is the stop Lazybox asked the provider for (Stop, Send now), keyed
// to the main agent's turn it was asked in: the running turn, or, between a
// delivered message and its turn's first event, the turn that message starts.
// How that turn ends is the interrupt, not a provider error (see stopReport);
// no other turn's is. A flag that outlived its turn (the stop found the turn
// already over, or the message it waited for never started one) would drop
// a later turn's real failure and end that turn as interrupted.
type turnStop struct {
	// turns counts the main agent's turns started; open says one runs.
	turns uint64
	open  bool
	// asked is whether a stop is pending, for turn.
	asked bool
	turn  uint64
}

// current is the turn events belong to now: the running one, else the next.
func (s *turnStop) current() uint64 {
	if s.open {
		return s.turns
	}
	return s.turns + 1
}

// ask records a stop for the current turn and returns that turn, for forget.
func (s *turnStop) ask() uint64 {
	s.asked, s.turn = true, s.current()
	return s.turn
}

// forget takes back the stop asked for turn (it failed); a later one stays.
func (s *turnStop) forget(turn uint64) {
	if s.asked && s.turn == turn {
		s.asked = false
	}
}

// clear drops any stop: a new native session, or nothing left to stop.
func (s *turnStop) clear() { s.asked = false }

// stopping reports whether the turn events now belong to is the stopped one.
func (s *turnStop) stopping() bool { return s.asked && s.turn == s.current() }

// started is the main agent's turn_started: a stop asked for an earlier turn
// is over.
func (s *turnStop) started() {
	s.turns++
	s.open = true
	if s.asked && s.turn < s.turns {
		s.asked = false
	}
}

// ended is the main agent's turn_ended. It reports whether that turn was the
// stopped one, and forgets the stop either way.
func (s *turnStop) ended() bool {
	stopped := s.stopping()
	if !s.open {
		s.turns++ // A turn that ended without a turn_started.
	}
	s.open, s.asked = false, false
	return stopped
}

// undelivered: the message whose turn a stop may wait for was not delivered,
// so that turn never starts.
func (s *turnStop) undelivered() {
	if !s.open {
		s.asked = false
	}
}

// adopted restores what a previous Runner left: whether a turn runs, and
// whether a stop it asked for is pending in it.
func (s *turnStop) adopted(open, stopping bool) {
	*s = turnStop{open: open}
	if stopping {
		s.ask()
	}
}
