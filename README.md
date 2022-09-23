# manini

ドケチのための ECS 用リバースプロキシ

## 特徴

- 軽量なリバースプロキシ (ALB よりも安い EC2 インスタンスで動かすことを想定しています)
- サービスの自動スケーリング
    - 一定時間リクエストがなければ、サービスを停止
    - リクエストが来たときに、サービスを起動
    - ブラウザからのアクセスには「しばらくお待ちください」メッセージを表示
- WebSocket 対応

## 前提条件

- network_mode が awsvpc
- healthCheck が設定されている

## 必要なポリシー

```json
{
    "Version": "2012-10-17",
    "Statement": [
        {
            "Effect": "Allow",
            "Action": [
                "ecs:UpdateService",
                "ec2:DescribeNetworkInterfaces",
                "ecs:ListTasks",
                "ecs:DescribeServices",
                "ecs:DescribeTasks"
            ],
            "Resource": "*"
        }
    ]
}
```

## 使い方

### コマンドラインから

```
manini --cluster your-cluster --service your-service --target-port 3000
```

### docker-compose

```yaml
version: '2.2'

services:
  manini:
    image: ghcr.io/azyobuzin/manini:main
    environment:
      - MANINI_CLUSTER=your-cluster
      - MANINI_SERVICE=your-service
      - MANINI_TARGET_PORT=3000
      - RUST_LOG=info,aws_smithy_http_tower=warn,aws_config=warn
    restart: unless-stopped
    network_mode: host
    mem_limit: 50MB
    cpus: 0.2
```

## 設定

| コマンドライン引数 | 環境変数 | 説明 |
| ---------------- | -------- | ---- |
| --cluster | MANINI_CLUSTER | クラスタ名。省略するとデフォルトクラスタ |
| --service | MANINI_SERVICE | サービス名 |
| --target-port | MANINI_TARGET_PORT | コンテナ側のポート番号 |
| --public-ip | MANINI_PUBLIC_IP | パブリックIPアドレスを使用するか。指定しない場合はプライベートIPアドレスを使用する |
| --scale_down_period | MANINI_SCALE_DOWN_PERIOD | 何秒間アクセスがなかったらサービスを停止するか。デフォルト 300 秒 |
| --bind | MANINI_BIND | プロキシがリッスンするアドレス。デフォルト 0.0.0.0:3000 |
| | RUST_LOG | ログ設定。設定方法は [env_logger のドキュメント](https://docs.rs/env_logger/0.9.1/env_logger/index.html#enabling-logging)を参照 |
