package integration

import (
	"context"
	"encoding/json"
	"net/url"
	"os"
	"testing"
	"time"

	pb "github.com/AntoineToussaint/lazybox/v2/pkg/gen"
	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
)

func TestRunningCodeflyServiceNativeGRPC(t *testing.T) {
	path := os.Getenv("LAZYBOX_V2_ENDPOINT")
	if path == "" {
		t.Skip("running Codefly service not selected; set LAZYBOX_V2_ENDPOINT")
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var endpoint struct {
		URL   string `json:"url"`
		Token string `json:"token"`
	}
	if err = json.Unmarshal(data, &endpoint); err != nil {
		t.Fatal(err)
	}
	address, err := url.Parse(endpoint.URL)
	if err != nil {
		t.Fatal(err)
	}
	connection, err := grpc.NewClient(address.Host, grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	client := pb.NewRunnerServiceClient(connection)
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if _, err = client.Info(ctx, &pb.InfoRequest{}); status.Code(err) != codes.Unauthenticated {
		t.Fatalf("unauthenticated native call was not refused: %v", err)
	}
	authorized := metadata.NewOutgoingContext(ctx, metadata.Pairs("authorization", "Bearer "+endpoint.Token))
	info, err := client.Info(authorized, &pb.InfoRequest{})
	if err != nil || len(info.Profiles) == 0 {
		t.Fatalf("native gRPC did not serve the application: %v", err)
	}
	t.Log("actual Codefly listener: native gRPC and shared authority interceptors verified")
}
