// Copyright (c) 2025 Beijing Volcano Engine Technology Co., Ltd. and/or its affiliates.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
package main

import (
	"context"
	"flag"
	"log"

	"github.com/volcengine/veadk-go/v2/apps"
	"github.com/volcengine/veadk-go/v2/apps/agentkit_server_app"
	"google.golang.org/adk/v2/agent"
)

func main() {
	host := flag.String("host", "127.0.0.1", "HTTP listen host")
	port := flag.Int("port", 8000, "HTTP listen port")
	flag.Parse()

	a, err := buildSampleAgent()
	if err != nil {
		log.Fatal(err)
	}
	config := apps.DefaultApiConfig().SetHost(*host).SetPort(*port).SetWebUIEnabled(false)
	app := agentkit_server_app.NewAgentkitServerApp(config)
	if err := app.Run(context.Background(), &apps.RunConfig{
		AgentLoader:          agent.NewSingleLoader(a),
		DisableObservability: true,
	}); err != nil {
		log.Fatal(err)
	}
}
