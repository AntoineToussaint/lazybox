package transport

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"net/http"
	"os"
	"regexp"
	"strings"
	"time"

	pb "github.com/AntoineToussaint/lazybox/services/runner/code/pkg/gen"
	"google.golang.org/grpc/codes"
)

const wisprEndpoint = "https://platform-api.wisprflow.ai/api/v1/dash/api"
const wisprKey = "LAZYBOX_WISPR_API_KEY"

var wisprLanguage = regexp.MustCompile(`^[a-z]{2}$`)
var wisprHTTP = &http.Client{
	Timeout: 20 * time.Second,
	CheckRedirect: func(_ *http.Request, _ []*http.Request) error {
		return errors.New("Wispr Flow redirects are not followed")
	},
}

func (s *Service) WisprStatus(_ context.Context, _ *pb.WisprStatusRequest) (*pb.WisprStatusResponse, error) {
	return &pb.WisprStatusResponse{Configured: strings.TrimSpace(os.Getenv(wisprKey)) != ""}, nil
}

func (s *Service) TranscribeWispr(ctx context.Context, r *pb.TranscribeWisprRequest) (*pb.TranscribeWisprResponse, error) {
	key := strings.TrimSpace(os.Getenv(wisprKey))
	if key == "" {
		return nil, grpcError(codes.FailedPrecondition, errors.New("configure a Wispr Flow API key on this Runner"))
	}
	if !validWisprWAV(r.Wav) {
		return nil, grpcError(codes.InvalidArgument, errors.New("Wispr Flow requires up to 60 seconds of 16 kHz mono PCM WAV audio"))
	}
	language, _, _ := strings.Cut(strings.ToLower(r.Language), "-")
	var languages []string
	if wisprLanguage.MatchString(language) {
		languages = []string{language}
	}
	text, err := requestWispr(ctx, wisprHTTP, wisprEndpoint, key, r.Wav, languages)
	if err != nil {
		return nil, rpcError(err)
	}
	return &pb.TranscribeWisprResponse{Text: text}, nil
}

// The browser sends the canonical 44-byte WAV header and 16-bit PCM samples.
func validWisprWAV(audio []byte) bool {
	if len(audio) <= 44 || len(audio) > 44+60*16000*2 || len(audio)%2 != 0 {
		return false
	}
	le := binary.LittleEndian
	return string(audio[:4]) == "RIFF" && le.Uint32(audio[4:]) == uint32(len(audio)-8) &&
		string(audio[8:16]) == "WAVEfmt " && le.Uint32(audio[16:]) == 16 &&
		le.Uint16(audio[20:]) == 1 && le.Uint16(audio[22:]) == 1 &&
		le.Uint32(audio[24:]) == 16000 && le.Uint32(audio[28:]) == 32000 &&
		le.Uint16(audio[32:]) == 2 && le.Uint16(audio[34:]) == 16 &&
		string(audio[36:40]) == "data" && le.Uint32(audio[40:]) == uint32(len(audio)-44)
}

func requestWispr(ctx context.Context, client *http.Client, endpoint, key string, audio []byte, languages []string) (string, error) {
	body, err := json.Marshal(struct {
		Audio    []byte   `json:"audio"`
		Language []string `json:"language,omitempty"`
	}{audio, languages})
	if err != nil {
		return "", err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, endpoint, bytes.NewReader(body))
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+key)
	req.Header.Set("Content-Type", "application/json")
	reply, err := client.Do(req)
	if err != nil {
		return "", errors.New("Wispr Flow could not be reached")
	}
	defer reply.Body.Close()
	switch reply.StatusCode {
	case http.StatusUnauthorized, http.StatusForbidden:
		return "", errors.New("Wispr Flow rejected the Runner's API key")
	case http.StatusTooManyRequests:
		return "", errors.New("Wispr Flow is rate limited; try again shortly")
	case http.StatusOK:
	default:
		return "", errors.New("Wispr Flow could not transcribe this recording")
	}
	var payload struct {
		Text *string `json:"text"`
	}
	if err := json.NewDecoder(io.LimitReader(reply.Body, 256<<10)).Decode(&payload); err != nil || payload.Text == nil {
		return "", errors.New("Wispr Flow returned an invalid transcription")
	}
	return strings.TrimSpace(*payload.Text), nil
}
