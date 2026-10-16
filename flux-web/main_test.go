package main

import (
	"sync"
	"testing"
)

func TestSessionMachineAccessIsSynchronized(t *testing.T) {
	session := &Session{}
	machine := &machineUpstream{}
	session.setMachine(machine)

	var wg sync.WaitGroup
	wg.Add(2)
	go func() {
		defer wg.Done()
		for range 1000 {
			session.setMachine(machine)
		}
	}()
	go func() {
		defer wg.Done()
		for range 1000 {
			if session.getMachine() != machine {
				t.Errorf("machine access returned a different machine")
				return
			}
		}
	}()
	wg.Wait()
}
