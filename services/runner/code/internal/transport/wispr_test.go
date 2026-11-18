package transport

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"testing"
)

func TestWisprWAVValidationAndRESTTranscription(t *testing.T) {
	wav := make([]byte, 46)
	copy(wav[0:], "RIFF")
	copy(wav[8:], "WAVEfmt ")
	copy(wav[36:], "data")
	le := binary.LittleEndian
	for offset, value := range map[int]uint32{4: 38, 16: 16, 24: 16000, 28: 32000, 40: 2} {
		le.PutUint32(wav[offset:], value)
	}
	for offset, value := range map[int]uint16{20: 1, 22: 1, 32: 2, 34: 16} {
		le.PutUint16(wav[offset:], value)
	}
	if !validWisprWAV(wav) || validWisprWAV(wav[:44]) {
		t.Fatal("WAV validation did not distinguish a sample from an empty recording")
	}

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost || r.Header.Get("Authorization") != "Bearer fixture-key" {
			t.Error("Wispr request did not carry Runner authentication")
		}
		var request struct {
			Audio    []byte
			Language []string
		}
		err := json.NewDecoder(r.Body).Decode(&request)
		if err != nil || !validWisprWAV(request.Audio) || len(request.Language) != 1 || request.Language[0] != "fr" {
			t.Errorf("Wispr request: %v, languages %v", err, request.Language)
		}
		fmt.Fprint(w, `{"text":" Bonjour. "}`)
	}))
	defer server.Close()
	if got, err := requestWispr(context.Background(), server.Client(), server.URL, "fixture-key", wav, []string{"fr"}); err != nil || got != "Bonjour." {
		t.Fatalf("transcript: %q, %v", got, err)
	}
}
