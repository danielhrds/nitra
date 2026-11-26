<p align="center"><img src="docs/logo.png" width="231" alt="nitra"></p>

# nitra

Compartilhamento de tela com baixa latência para Windows, feito em Rust para poucos amigos de cada vez. Você transmite um jogo ou uma janela numa sala; até três amigos assistem ao vivo, ponto a ponto, enquanto o jogo continua rodando liso.

> **Status:** beta privada (`0.1.0-beta.3`). Desenvolvido numa AMD Radeon RX 7600 com Windows 11; usuários da beta já transmitiram e assistiram com uma RTX 2060, uma RTX 3050 e uma RX 6750 XT. Intel é experimental (veja [Hardware](#hardware)).

**Leia em:** [English](README.md) · Português (Brasil)

## Recursos

- **Crie uma sala** e mande o convite; todo mundo nela vê quem está online e quem está ao vivo. Várias pessoas podem transmitir ao mesmo tempo, e dá para assistir a mais de uma tela lado a lado, destacar uma ou ir para tela cheia.
- **Amigos**, salas de duas pessoas numa aba própria: adicione alguém com um convite de uso único, ou peça amizade a alguém de uma sala. Qualquer um dos dois pode desfazer, para os dois.
- **Uma câmera ao lado da tela.** Cada pessoa pode transmitir uma tela e uma câmera, cada uma no seu card: assista às duas ao mesmo tempo (até da mesma pessoa), destaque uma, deixe uma flutuando sobre o jogo. Funciona qualquer câmera que o Windows ofereça, pelo Media Foundation ou pelo DirectShow (a OBS Virtual Camera, o Iriun e afins), em até 720p30. Ela nunca liga sozinha, e enquanto está ligada um selo vermelho "Câmera ligada" fica à vista, inclusive em tela cheia.
- **Clique num card para assistir**, ajuste o volume de cada tela (lembrado por pessoa) e deixe qualquer tela ou câmera assistida numa janelinha flutuante por cima de todas as outras, inclusive do jogo, para arrastar e redimensionar onde quiser.
- **Transmita uma tela ou uma única janela**, com o som dela: uma janela transmite só o áudio do próprio app; uma tela inteira transmite o som do sistema sem o Discord, o nitra e os apps que você excluir. **Troque** para outra tela ou janela sem encerrar a transmissão: quem assiste continua conectado e a imagem muda em cerca de 0,2 s.
- **Veja a sua própria transmissão**, como no OBS ou no Discord: uma prévia ao vivo opcional no seu card, desligada por padrão. O painel de transmitir pode ser escondido para dar mais espaço às telas.
- **Predefinições de qualidade:** 720p60 a 6 Mbps (o padrão), 1080p60, 1080p30 (bom para filmes) e 1440p60, além de um 720p30 econômico a 2 Mbps para uploads lentos, com um ajuste avançado de taxa de bits. A imagem mantém as proporções quando a fonte muda de formato.
- **Suave ou imediato:** cada espectador escolhe atraso mínimo, suave (uma espera de até dois quadros) ou automático, que espera só o quanto a rede precisa.
- **Prévias cifradas** de cada tela ao vivo para quem está na sala e não está assistindo.
- **Números ao vivo** em cada tela assistida: taxa de quadros, taxa de bits, perda de pacotes, tempo de ida e volta e o caminho da conexão (rede local, internet, IPv6). Quem transmite vê quem está assistindo, com a perda e o tempo de ida e volta de cada um. Quando a conexão direta falha, o app diz por quê.
- **Fecha como um app normal.** O X fecha o nitra; enquanto ele roda, um ícone na bandeja esconde e mostra a janela com um clique. Se preferir, ele também pode ficar na bandeja ao fechar.
- **Uma janela por perfil**, e mensagens claras quando uma janela transmitida fecha, um monitor some ou o PC suspende.
- **Se recupera sozinho** de uma GPU perdida (reinício do driver), da suspensão e de mudanças de tela.
- **Em português ou inglês**, seguindo o Windows ou escolhido nas configurações.

## Instalar, usar e remover

1. Baixe o `nitra.exe` e confira o SHA-256 dele com o que foi publicado junto. É um arquivo só, com o runtime do C embutido; não há instalador.
2. Abra. O Windows pode mostrar "O Windows protegeu o computador" porque o arquivo não é assinado: clique em **Mais informações → Executar assim mesmo**.
3. Digite o seu nome, crie uma sala e copie o convite para os seus amigos. Eles colam no nitra deles para entrar.

O nitra guarda os dados numa pasta só, `%LOCALAPPDATA%\nitra`: a chave do aparelho (protegida pelo DPAPI do Windows), as suas salas, as suas configurações e os últimos dez logs. Ele não grava mais nada: nem chaves de registro, nem arquivos temporários, nem cache. As configurações mostram a versão e têm o botão **Abrir pasta de logs** para quando algo der errado.

**Para remover o nitra,** use **Apagar meus dados deste PC** nas configurações (ele pergunta antes, fecha o nitra e apaga essa pasta) e depois apague o `nitra.exe`. O próprio Windows pode manter uma regra de firewall se você liberou o nitra no firewall; ela é inofensiva.

## Hardware

| GPU | Transmitir (codificar) | Assistir (decodificar) |
| --- | --- | --- |
| AMD | **Testada** (AMF; RX 7600, RX 6750 XT) | **Testada** (decodificador H.264 do Windows, DXVA) |
| NVIDIA | **Testada** (Media Foundation / NVENC; RTX 2060, RTX 3050) | **Testada** (decodificador H.264 do Windows, DXVA) |
| Intel | Experimental (Media Foundation / Quick Sync) | Decodificador H.264 do Windows (DXVA) |

As medições abaixo são da RX 7600; as outras placas foram testadas por usuários da beta, transmitindo e assistindo, sem benchmark. O nitra ainda pergunta antes da primeira transmissão na NVIDIA e na Intel. O nitra nunca codifica por software, porque isso pesaria no jogo; sem um codificador H.264 de hardware, ele avisa e sugere atualizar o driver.

## Limitações conhecidas

- **Notebooks com duas GPUs:** o nitra usa a primeira, que pode ser a integrada enquanto o jogo roda na outra.
- **Sem servidor de relay:** quando um NAT restrito ou um firewall bloqueia a conexão direta dos dois lados, a transmissão não começa; o app diz qual lado é a causa provável. O `nat-probe.exe` (neste repositório) testa uma rede em cerca de 25 segundos e diz se uma conexão direta é provável, sem guardar o endereço IP público.
- **Muita perda de pacotes:** com 5–10% de perda, os keyframes em 1080p podem perder pacotes que não são recuperados, e a imagem pode congelar por alguns segundos (veja [Rede](#rede)).
- **A janela flutuante** fica acima das outras janelas, mas um jogo em tela cheia exclusiva a cobre (um limite do Windows); tela cheia sem bordas funciona.
- Ainda sem teste de estresse em sessões de várias horas ou em centenas de ciclos de iniciar e parar.

## Como funciona

### A imagem e o som

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/flow/media-pt-dark.svg">
  <img src="docs/flow/media-pt-light.svg" width="420" alt="Captura, limitador de taxa, conversão na GPU e codificador de hardware em quem transmite; captura de som e Opus ao lado; uma fila de envio por espectador; decodificador de hardware, janela e player de som em quem assiste">
</picture>

Os quadros ficam na GPU, como texturas D3D11, da captura até o codificador; nenhum pixel passa pela memória da CPU. Uma codificação serve a todos os espectadores. Vídeo e som compartilham um mesmo relógio, então ficam sincronizados em quem assiste.

### Entrando numa sala

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/flow/rooms-pt-dark.svg">
  <img src="docs/flow/rooms-pt-light.svg" alt="Quem transmite entra assinando um desafio com a chave do aparelho, cria uma sala e recebe um convite; quem assiste entra com o convite e um desafio assinado">
</picture>

Não há contas nem senhas. O servidor das salas é um Cloudflare Worker com um Durable Object por sala; ele cuida da entrada, da presença, da sinalização e das prévias lacradas, nunca da mídia.

### Começando a assistir

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/flow/watch-pt-dark.svg">
  <img src="docs/flow/watch-pt-light.svg" alt="Oferta e resposta assinadas, repassadas pelo servidor; testes ICE por UDP direto; handshake DTLS; depois mídia cifrada ponto a ponto">
</picture>

### Prévias

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/flow/preview-pt-dark.svg">
  <img src="docs/flow/preview-pt-light.svg" alt="Uma imagem pequena e parada vira um JPEG, é lacrada para cada membro da sala, repassada pelo servidor e aberta pelos membros">
</picture>

Os diagramas são desenhados com o Excalidraw; as fontes estão em [`docs/flow`](docs/flow).

### Decisões principais

- **Windows Graphics Capture** em vez do Desktop Duplication: o mesmo impacto no jogo, ou menor, com um terço da CPU.
- **Um shader na GPU** para conversão de cor e escala: mais rápido que o video processor (0,22% contra 0,30% da GPU em 1080p60; 0,14% contra 0,62% em 720p).
- **AMF** como codificador na AMD: 2,3 ms contra 5,9 ms do Media Foundation, que continua sendo o caminho nas outras GPUs.
- **str0m** para o WebRTC em vez do crate `rtc`: com 2% de perda de pacotes, entregou 1.789 de 1.800 quadros a 1,0 ms (p50), contra 1.256 a 900 ms, e quem entrou depois recebeu um keyframe em 101 ms em vez de 493 ms.
- **Dear ImGui** para a interface em vez do GPUI: dorme quando nada muda (0% de CPU contra 1,2%, que acordava cerca de 650 vezes por segundo), abre duas vezes mais rápido e não tem custo mensurável no jogo, enquanto o GPUI custava 4% do 1% low.
- **Ponto a ponto, 1–3 espectadores,** sem servidor de mídia e sem relay: nada para pagar ou manter além do Worker gratuito.
- **Sem codificador por software como reserva:** ele pesaria na CPU e passaria pixels pela memória enquanto o jogo roda.

## Desempenho desde o projeto

O objetivo é um jogo que continua liso enquanto é transmitido. Cada escolha abaixo foi medida, em A/B, contra a alternativa.

**Captura e codificação**
- **Só GPU, da captura ao codificador.** Os quadros ficam como texturas D3D11; conversão de cor e escala rodam numa única passada de shader para um conjunto fixo de texturas NV12.
- **Capturar só o que é enviado.** O Windows Graphics Capture é pedido na taxa alvo, calculada a partir da taxa de atualização do monitor capturado, em vez de todo quadro que o jogo desenha (cerca de 114 por segundo a 60 fps).
- **60 fps estáveis.** Um limitador de taxa por créditos absorve as rajadas na entrega de quadros do jogo, então nenhum quadro da fonte é descartado e quem assiste recebe ~59,9 fps estáveis.
- **Codificação assíncrona com filas limitadas.** No máximo 4 quadros ficam no codificador ao mesmo tempo; toda fila tem tamanho fixo, então nada cresce sem limite sob carga.
- **Imagem na hora para quem chega depois.** Quando a fonte está minimizada ou parada, o último quadro é codificado de novo a pedido, então um espectador novo nunca espera numa tela preta.

**Rede**
- **Uma codificação para todos.** Cada espectador só adiciona o custo de enviar os pacotes: cerca de 4% de um núcleo, mais ou menos 0,25% do processador inteiro.
- **Conexões lentas não seguram as outras.** Cada espectador tem um orçamento de envio de 300 ms; um espectador numa conexão lenta pula quadros e volta no próximo keyframe, enquanto o codificador acompanha o espectador mais lento que consegue acompanhar. Os keyframes são limitados a um a cada 500 ms, não importa quantos espectadores peçam.
- **Retransmissões numa janela larga.** Pacotes perdidos são pedidos de novo ao longo do último ~1 s de vídeo, em vez dos ~110 ms do str0m, então 2–5% de perda são corrigidos sem keyframe nem congelamento.
- **Som que recupera o atraso.** Opus com correção de erros e ocultação de perdas. O som atrasado é limitado a 40 ms, e depois de uma travada da rede o player descarta o acumulado de uma vez, em vez de voltar aos poucos ao longo de meio minuto.
- **Pacotes malformados ou grandes demais são descartados,** sem nunca encerrar a sessão.

**Assistir**
- **Um dispositivo de vídeo para todas as telas assistidas.** Os decodificadores rodam no próprio dispositivo D3D11 da janela, em vez de um cada, então uma tela assistida custa cerca de 50 MB em vez de cerca de 80 MB, sem CPU a mais.
- **Menos superfícies no decodificador.** Os dois codificadores preveem cada quadro só a partir do anterior, mas declaravam quatro quadros de referência; o decodificador do Windows calcula as superfícies a partir desse número (referências + 7). Declarar um dá as mesmas imagens, bit a bit, e 8 superfícies por tela assistida em vez de 11: 9 MB a menos de memória de vídeo em 720p e 19 MB a menos em 1080p.
- **Telas escondidas não são desenhadas.** Quando uma tela assistida está minimizada ou coberta, o desenho para: 0% de carga 3D, 40% menos despertares e 14 MB a menos de memória de vídeo.
- **A janela passa pelo compositor da área de trabalho** (DirectComposition), então uma janela maximizada nunca toma a taxa de atualização do monitor e o mouse continua liso.
- **Nada roda quando nada muda.** A janela dorme até haver entrada ou um quadro novo: 0% de CPU em repouso, e ela abre em cerca de 106 ms.

**Servidor das salas**
- **As salas dormem entre as mensagens.** Cada sala é um Durable Object com WebSockets que hibernam; os pings de manutenção são respondidos pelo runtime sem acordar a sala, então uma sala quieta custa uma requisição por minuto.

## Benchmarks

Medido num Ryzen 7 5800XT (16 threads) com uma RX 7600, um monitor principal de 320 Hz e um segundo monitor de 144 Hz. Os resultados no jogo vêm do Campo de Treino do Valorant, em rodadas repetidas e alternadas, comparadas com intervalos de confiança de 95%; "sem diferença mensurável" quer dizer que o intervalo inclui o zero. A CPU é uma porcentagem de um núcleo, com a parcela do processador inteiro entre parênteses quando importa. A metodologia está em [`docs/benchmark-protocol.md`](docs/benchmark-protocol.md) (em inglês).

### Impacto no jogo

Cada etapa da transmissão, adicionada uma de cada vez (1080p, 10 Mbps, sem limite de FPS, OBS fechado):

| Condição | FPS médio | 1% low | CPU do processo |
| --- | --- | --- | --- |
| Sem captura | 635,4 | 398,1 | — |
| Só captura | 601,2 (−5,4%) | 366,2 | 2,5–3,0% |
| Captura + conversão na GPU | 600,6 (−5,5%) | 371,5 | 4,4–5,0% |
| Captura + conversão + codificação AMF | 594,9 (−6,4%) | 359,7 | 7,7–8,7% |

Quase todo o custo é a própria captura: o jogo demora mais para entregar cada quadro ao Windows enquanto é capturado. Conversão e codificação não fazem diferença mensurável na média.

Windows Graphics Capture contra Desktop Duplication, na mesma sessão (o OBS provavelmente estava aberto, o que baixa a base):

| Captura | FPS médio | 1% low | CPU do processo de captura |
| --- | --- | --- | --- |
| Nenhuma | 569,7 | 342,7 | — |
| Windows Graphics Capture | 560,5 (−1,6%, sem diferença mensurável) | 329,3 | 3,0% (0,19%) |
| Desktop Duplication | 558,0 (−2,1%, mensurável) | 329,3 | 9,8% (0,61%) |

### Taxa de captura

| Quadros pedidos ao Windows Graphics Capture | Capturados por segundo | Chegam ao codificador | FPS do jogo | CPU do processo |
| --- | --- | --- | --- | --- |
| O dobro da taxa alvo | 114 | 59,9 fps | 597,4 | 7,9–9,5% |
| Na taxa alvo | 63,4 | 57,6 fps | 603,4 (+1,0%) | 5,8–8,3% |

Pedir só a taxa alvo é mais barato para o jogo, mas, num monitor de 320 Hz, perdia quadros por arredondamento. A regra final (90% do menor intervalo entre quadros que o monitor permite, com um limitador por créditos) mantém a taxa menor e a taxa de quadros cheia: os quadros da fonte descartados foram de 109–135 por minuto para **0**, e quem assiste recebe 59,9 fps estáveis.

### Transmitindo enquanto joga

A transmissão completa (captura, conversão, codificação e envio para um espectador em outro PC) em 720p60, 6 Mbps:

| Cenário | FPS médio | 1% low | Custo |
| --- | --- | --- | --- |
| Sem limite, sem OBS | 629 → 594 | 395 → 364 | −5,6% na média, −7,8% no 1% low |
| Sem limite, replay buffer do OBS rodando | 540 → 522 | 302 → 265 | −3,3% na média, −12% no 1% low |
| **FPS limitado a 300**, replay buffer do OBS rodando | 299,8 → 299,6 | 223 → 218 | **sem diferença mensurável** |

Com o jogo limitado, como a maioria joga, a GPU tem folga: a transmissão só adiciona cerca de 3% ao tempo de GPU do jogo por quadro e a taxa de quadros não se mexe.

### Assistindo enquanto joga

O espectador (decodificação por hardware e apresentação numa janela de 1280×720 no segundo monitor) enquanto joga no principal:

| Transmissão | Sem limite: FPS médio | Sem limite: 1% low | Limitado a 300: 1% low |
| --- | --- | --- | --- |
| Nada | 613,9 | 379,4 | 232,3 |
| 1080p30 | 610,9 (sem diferença mensurável) | 369,8 (−2,5%) | 230,8 (sem diferença mensurável) |
| 1080p60 | 608,4 (sem diferença mensurável) | 365,3 (−3,7%) | 227,5 (sem diferença mensurável) |
| 720p60 | 604,3 (−1,6%) | 360,4 (−5,0%) | 226,6 (−2,5%, no limite) |

Limitado a 300, a média ficou em 299,8 em todos os casos. O custo cresce com o número de quadros mostrados por segundo, não com a resolução.

### Junto com o OBS

O mesmo codificador da GPU dividido com um replay buffer do OBS (AV1, 1080p60, 6,5 Mbps, predefinição de alta qualidade), dois minutos cada, com o nitra transmitindo o Valorant em 720p60 para uma janela no segundo monitor:

| | Só o OBS | OBS + nitra |
| --- | --- | --- |
| Carga do codificador de vídeo da GPU | 72,0% (pico 76,3%) | 60,6% (pico 61,0%) |
| Carga 3D da GPU | 47,4% | 48,0% |
| Quadros gravados / renderizados pelo OBS | 9.091 / 9.102 | 10.424 / 10.436 |
| Atraso de codificação do OBS | nenhum | nenhum |
| nitra | — | 60 fps, 0 descartados, maior intervalo 59 ms |

O jogo com o OBS gravando, sem e com o nitra transmitindo (720p60, 6 Mbps):

| Cenário | FPS médio | 1% low | Tempo de CPU / GPU do jogo por quadro |
| --- | --- | --- | --- |
| Sem limite, transmitindo o monitor | 539,6 → 521,5 (−3,35%) | 302,4 → 265 (−12,4%) | +3,2% / +4,2% |
| Sem limite, transmitindo o monitor (repetido) | 539,5 → 521,6 (−3,31%) | 299,8 → 261 (−12,8%) | — |
| Sem limite, transmitindo a janela do jogo | 539,5 → 521,9 (−3,26%) | 299,8 → 267 (−11,1%), mais estável | — |
| Limitado a 300, transmitindo a janela | 299,8 → 299,6 (sem diferença mensurável) | 223,0 → 218 (sem diferença mensurável) | GPU +2,8% |

Capturar a janela do jogo ou o monitor inteiro custa o mesmo; a janela deu lows um pouco mais estáveis.

### Taxa de bits: 6 Mbps ou 2 Mbps

720p60 a 6 Mbps (o padrão) contra 720p30 a 2 Mbps (a predefinição econômica), num trecho sem perdas do Campo de Treino:

| Ajuste | VMAF | VMAF, piores 5% | CPU de quem transmite | CPU de quem assiste | Custo no FPS do jogo |
| --- | --- | --- | --- | --- | --- |
| 720p60 · 6 Mbps | **94,4** | 86,8 | 7,4% (0,46%) | 3,5–4,2% (~0,24%) | −5,6% |
| 720p60 · 3 Mbps | 82,5 | 67,3 | — | — | — |
| 720p60 · 2 Mbps | 70,5 | 48,4 | — | — | — |
| 720p30 · 2 Mbps | 84,2 | 69,3 | 3,3% (0,21%) | 1,6–2,2% (~0,12%) | −4,9% |
| 720p30 · 1,5 Mbps | 76,5 | 57,4 | — | — | — |

O VMAF dá uma nota para o quanto a imagem se parece com a original (acima de ~93 parece igual). Com pouca taxa de bits, 30 fps mantém cada quadro mais nítido que 60 fps; o jogo paga mais ou menos o mesmo nos dois casos, então o padrão fica em 6 Mbps e a predefinição de 2 Mbps existe para uploads lentos. A memória é a mesma nos dois. Na rede, os cabeçalhos dos pacotes adicionam cerca de 1,5% à taxa de bits.

### Codificador

| Codificador | Latência p50 / p99, 1080p 10 Mbps | Latência p50 / p99, 1080p 20 Mbps | CPU para enviar um quadro |
| --- | --- | --- | --- |
| AMF | **2,3 / 2,9 ms** | 2,3 / 2,6 ms | 177 µs |
| Media Foundation | 5,9 / 6,2 ms | 5,9 / 6,0 ms | 281 µs |

Os dois atingem a taxa de bits alvo (com menos de 1% de diferença) sem descartar nenhum quadro. Qualidade num trecho de jogo com a mesma taxa de bits (Media Foundation / AMF):

| Saída | PSNR-Y (dB) | SSIM | VMAF |
| --- | --- | --- | --- |
| 1080p · 6 Mbps | 36,2 / 36,2 | 0,951 / 0,950 | 82,8 / 82,2 |
| 1080p · 10 Mbps | 39,0 / 38,9 | 0,967 / 0,967 | 92,5 / 92,0 |
| 1080p · 20 Mbps | 42,8 / 42,6 | 0,982 / 0,981 | 98,4 / 98,2 |
| 720p · 6 Mbps | 39,6 / 39,5 | 0,969 / 0,969 | 94,7 / 94,4 |

Os dois codificadores rodam no mesmo hardware e dão a mesma qualidade; o AMF ganha na latência e evita o "modo lento" intermitente do Media Foundation (cerca de 30 ms por quadro). Captura e codificação juntas no jogo, 1080p 10 Mbps:

| OBS | AMF | Media Foundation |
| --- | --- | --- |
| Replay buffer rodando | −2,9% no FPS médio | −4,6% |
| Fechado | −7,0% | −7,4% |

### Conversão e decodificação

| Etapa | Resultado |
| --- | --- |
| Shader BGRA → NV12, 1080p | 36,6 µs de GPU por quadro (0,22% da GPU a 60 fps) |
| Shader BGRA → NV12, 720p com escala | 23,4 µs (0,14%) |
| Decodificação por hardware, 1080p 10 Mbps | 234 µs p50, 476 µs p99 por quadro; até cerca de 930 quadros por segundo |
| Decodificação por hardware, 720p 6 Mbps | 207 µs p50 |

### De ponta a ponta

Do momento em que um quadro é renderizado até ele chegar à tela, no mesmo PC (1080p 10 Mbps, medido com o PresentMon na tela):

| Codificador | Renderizado → na tela, p50 | p95 | Decodificado → na tela, p50 |
| --- | --- | --- | --- |
| AMF | **10,3–11,2 ms** | 23–24 ms | 3,3 ms |
| Media Foundation | 13,4–14,7 ms | 26–27 ms | 3,4 ms |

Entre dois processos no mesmo PC, renderizado → decodificado é 10,2 ms p50 contra 9,7 ms dentro de um processo só: o caminho pela rede adiciona menos de 1 ms.

### Custo de quem transmite por espectador (1080p60, 10 Mbps)

| Espectadores | CPU de quem transmite | Carga do codificador de vídeo | Renderizado → decodificado, p50 |
| --- | --- | --- | --- |
| 1 | 5–6% (0,3–0,4%) | 9,5% | 4,8 ms |
| 3 | 13–15% (0,8–0,9%) | 9,5% | 5,7 ms |
| 5 | 21–23% (1,3–1,4%) | 9,3% | 7,3 ms |

A codificação roda uma vez para todos. Cada espectador adiciona cerca de 4% de um núcleo, mais ou menos 0,25% do processador inteiro, quase tudo na chamada de sistema que envia cada pacote.

### Rede

Recuperação numa rede com problemas (1080p60, 10 Mbps, duas rodadas):

| Condição | Quadros decodificados | Maior congelamento |
| --- | --- | --- |
| 1% de perda | 97–100% | 34–84 ms |
| 5% de perda | 83–87% | 1,5–2,4 s |
| 10% de perda | 46–65% | 2,7–4,3 s |
| Rajadas de perda | 96–99% | 0,2–0,65 s |
| Queda de 2 s | 100% | volta cerca de 40 ms depois que a rede volta |
| Atraso de 50 ± 20 ms, 2% fora de ordem | 98–100% | em ordem |

Nenhum quadro chegou corrompido em nenhuma condição. A perda de pacotes mostrada no app bate com a perda simulada (5,36% vistos para 5% simulados). Essas rodadas são de antes da janela de retransmissão mais larga; com ela, 2–5% de perda já não congelam a imagem (veja [Perda de pacotes](#perda-de-pacotes) abaixo).

### Som

| Medida | Resultado |
| --- | --- |
| Do som ao ouvido de quem assiste | 56,5 ms p50 (61 ms p95) |
| Som em relação à imagem | 33 ms depois dela |
| Do som ao decodificado, 5% de perda | 7 ms p50, 48–67 ms p95 |
| Sincronizado de novo depois de uma queda de 2 s | na hora (uma falha) |
| Codificar / decodificar Opus, por bloco de 20 ms | 166 µs / 42 µs |
| Player em 10 minutos | 0 falhas de buffer, 1,9% de um núcleo, 3,3 MB |
| Cada app capturado | cerca de 0,15% de um núcleo (0,01%), 0,1 MB |

### Quem assiste

O custo de quem assiste por transmissão, fora do jogo (a decodificação roda no motor de vídeo separado da GPU):

| Transmissão | GPU 3D | Decodificação de vídeo na GPU | CPU | Memória de vídeo |
| --- | --- | --- | --- | --- |
| 1080p60 | 1,0% | 12,9% | 0,3–5,6% | 112 MB |
| 1080p30 | 0,5% | 7,2% | 0,3–1,1% | 112 MB |
| 720p60 | 0,5% | 7,0% | 2,3–3,5% | 57 MB |
| 720p30 | 0,3% | 3,7% | 0,3–1,2% | 57 MB |

Memória por tela assistida, antes e depois de dividir um mesmo dispositivo de vídeo:

| | Antes | Depois |
| --- | --- | --- |
| Uma tela | 137 MB | 106 MB |
| Duas telas | 217 MB | 159 MB |
| Cada tela a mais | +80 MB, +699 handles, +26 threads | +53 MB, +105 handles, +10 threads |

Declarando um quadro de referência em vez de quatro (o resto do stream é igual: os dois arquivos diferem em 2 bytes, e VMAF, PSNR e latência são idênticos), um espectador com a sua janela, em rodadas alternadas:

| Transmissão | Memória de vídeo | Commit | "Memória" do Gerenciador de Tarefas | Fluidez |
| --- | --- | --- | --- | --- |
| 720p60, 6 Mbps | 58,8 → **49,8 MB** | 76,6 → 71,8 MB | 25,5 MB, sem mudança | todo quadro decodificado mostrado, maior intervalo 33–34 ms nos dois |
| 1080p60, 10 Mbps | 113,8 → **95,6 MB** | 111,3 → 102,3 MB | 25,9 MB, sem mudança | todo quadro decodificado mostrado, maior intervalo 34–35 ms nos dois |

A coluna "Memória" do Gerenciador de Tarefas é o *private working set*: a memória de fato em uso. O commit também conta a memória que o driver reserva e quase nunca usa, por isso é maior. Assistir a uma tela 720p no app leva o Gerenciador de 24 para 33 MB (commit de 42 para 95 MB, memória de vídeo de 20 para 68 MB).

A CPU ficou igual (6,3% de um núcleo com duas telas, em rodadas alternadas), com 0 quadros descartados. A janela mostrada pelo compositor da área de trabalho custa menos que uma presa à janela: 2,1% de um núcleo em vez de 2,9% enquanto assiste, sem mudança no jogo (−0,09%).

### O app

| Cenário | Resultado |
| --- | --- |
| Em repouso: dentro ou fora de uma sala, minimizado ou na bandeja | 0% de CPU, 56 MB |
| Abrir a janela | cerca de 106 ms |
| Transmitindo 720p60 | 2,3–4,3% de um núcleo (0,15–0,27%), 133 MB |
| Transmitindo e assistindo ao mesmo tempo | 8–11% de um núcleo (0,5–0,7%), 217 MB |
| Fechar transmitindo e assistindo | 0,3–0,4 s |
| Abrir de novo o mesmo perfil | traz a janela aberta para a frente em 77 ms |
| 10 ciclos de iniciar e parar de transmitir ou assistir | memória e handles voltam ao início: sem vazamento |
| Cada sala ou amigo salvo, em repouso | 0% de CPU, cerca de 1,3 MB (11 salas: +14 MB) |
| Trocar o que é transmitido | 60–160 ms em quem transmite; quem assiste continua conectado, com o maior intervalo de 183–264 ms (46–49 ms sem troca) e nenhum quadro descartado |
| Prévia da própria transmissão | cerca de +0,5% de um núcleo (dentro do ruído), +7 MB de memória e +5 MB de memória de vídeo; desenhada pela própria janela de vídeo, então a interface não redesenha por causa dela |
| Janela flutuante | sem custo extra: a mesma janela de vídeo, levada para cima das outras |

Contra a 0.1.0-beta.2, compilada da sua tag e rodada alternadamente nos mesmos cenários (3 rodadas de 60 s), nada cresceu: a janela em repouso e a transmissão 1080p60 para um espectador usam a mesma memória (36, 206 e 114 MB) e a mesma CPU dentro do ruído de uma rodada para outra (quem transmite 10,1% contra 10,8% de um núcleo em média, quem assiste 5,8% contra 4,5%).

### Câmera

| Cenário | Resultado |
| --- | --- |
| Captura, Iriun Webcam 720p30 (YUY2) | 30 quadros/s, nenhum descartado; 0,9–1,9% de um núcleo (0,06–0,12%); 0,19 ms por quadro (mediana) |
| Captura, OBS Virtual Camera (só oferece 1080p60, mantida em 30) | a nossa parte 0,94% de um núcleo; o próprio filtro do OBS, que roda dentro do nitra, 9% (desenhando a tela de espera) |
| Transmitindo uma câmera (captura, conversão na GPU, codificação, envio), Iriun | 1,5% de um núcleo (0,09%), 87 MB |
| Assistindo a uma câmera | 3,2–3,4% de um núcleo (0,2%), 113–121 MB |
| Assistindo a uma tela e a uma câmera juntas | 4,9–6,0% de um núcleo (0,31–0,37%), 149 MB: menos que as duas separadas, que dividem o trabalho da própria janela |
| Desligando a câmera | volta a 0% de CPU e à memória de antes de ligar |

Uma câmera entrega os quadros na memória, então cada quadro é copiado uma vez para a GPU e convertido lá (YUY2, NV12, RGB24 e RGB32), depois codificado como uma tela. O ritmo da própria câmera é respeitado: o limitador de taxa só afina uma câmera mais rápida que o pedido, porque os quadros que vêm pela rede (o Iriun) chegam em grupos que o limitador cortaria.

### Servidor das salas

| Medida | Resultado |
| --- | --- |
| Entrar na Cloudflare, com a sala começando do zero | 0,91 s |
| Oferta e resposta pela internet | 330 ms |
| Alguém sai sem avisar | os outros veem em 160 ms |
| Uma sala quieta | hiberna; acorda duas vezes por minuto enquanto alguém está conectado |
| Prévia cifrada | 1–3 ms para fazer e lacrar; até 22 KB para dois membros |
| Produção, de ponta a ponta | 479–482 quadros em 8 s pelo Worker publicado |

## Medições

Estas medições conferem o que quem assiste vê: se os quadros chegam, com que regularidade chegam à tela e como a transmissão aguenta uma rede ruim. Mesmo PC de antes; quem transmite e quem assiste no mesmo PC, por loopback, salvo quando dito o contrário, com os problemas de rede simulados do lado de quem assiste com uma semente fixa, para que as rodadas possam ser comparadas.

### O que o app registra

Enquanto transmite ou assiste, o nitra grava uma linha no log local a cada 10 segundos (cerca de 110 KB por hora por tela assistida; nada sai do PC). O botão **⋯** do card mostra os mesmos números sobre o vídeo.

| Lado | Números |
| --- | --- |
| Transmitindo | quadros capturados, codificados e enviados por segundo; quadros perdidos antes de codificar ou antes de enviar; taxa de bits |
| Assistindo | quadros recebidos, decodificados e mostrados por segundo; quadros descartados e substituídos; espaçamento na tela (mediana, percentil 95, o maior) e a parcela de quadros longos (mais de 1,5× a mediana); resolução, Mbps, perda, tempo de ida e volta |
| Assistindo, som | quanto tempo o som leva de chegar até ser ouvido (fila, buffer da placa de som e a latência do próprio aparelho); correções, pacotes ocultados, falhas, falhas de buffer |

A taxa de quadros do card conta os quadros mostrados, não os decodificados. O espaçamento na tela vem do sinal de latência de quadros da swap chain: as estatísticas de quadros que uma swap chain em janela recebe do DXGI seguem a tela principal, casando cerca de 90% dos quadros nela e cerca de 1% num segundo monitor com outra taxa de atualização. Comparado com elas, no monitor principal, o sinal dá a mesma mediana e o mesmo percentil 95 (16,3 contra 15,6 ms, 33,2 contra 34,4 ms).

### Quadros de um jogo de verdade

A janela do Valorant transmitida em 1080p60, 10 Mbps, para um espectador decodificando no mesmo PC (monitor de 320 Hz), 120 s:

| Etapa | Resultado |
| --- | --- |
| Capturados, codificados, enviados | 59,6–60,2 fps em cada linha de 10 s, nada perdido |
| Recebidos, decodificados | 59,8–60,2 fps, 7.207 quadros, 0 descartados |
| Maior intervalo | 81 ms, uma vez |

A janela sintética de teste usada nas outras medições não chega à própria tela a 60 fps regulares num monitor de 320 Hz (7,7% de quadros longos antes de qualquer captura); a captura entrega o que está na tela, sem nenhum quadro descartado pelo limitador.

### Som numa sessão longa

30 minutos de imagem e som sintéticos, com quem assiste tocando o som:

| Medida | Resultado |
| --- | --- |
| Da chegada até ser ouvido | 45,0–46,1 ms do começo ao fim |
| Som em relação à imagem | 34,7 ms depois dela |
| Correções, pacotes ocultados, falhas | 0 |
| Falhas de buffer | 1, no começo |
| Desvio do relógio da placa de som | +48 ppm, compensado |

O player nunca segura mais que o seu alvo de 60 ms mais 40 ms: o que passa disso é descartado na hora, então o atraso não cresce com o tempo.

### Perda de pacotes

1080p60, 10 Mbps, 60 s por rodada, antes e depois da janela de retransmissão mais larga (duas rodadas em cada taxa de perda):

| Condição | Quadros pulados | Pedidos de keyframe | Maior congelamento | FPS mostrado |
| --- | --- | --- | --- | --- |
| Sem problemas | 0 | 1 | 34–42 ms | 58–60 |
| 2% de perda, antes | 78–194 | 8–15 | 0,15–1,5 s | 46–57 |
| **2% de perda, depois** | **0** | **1** | **35–192 ms** | 48–57 |
| 5% de perda, antes | 466–505 | 35–38 | 1,1–1,6 s | 26–52 |
| **5% de perda, depois** | **0–2** | **1–2** | **35–68 ms** | 48–52 |
| Rajadas de perda (10 pacotes, ~1,7%) | 3 → 0 | 2 → 1 | 83 → 59 ms | 56–58 |
| Queda de 2 s | 6–25 | 2 | a própria queda | volta na hora |

Um espectador que só recebe não tem tempo de ida e volta pelo RTP, então o str0m repete o pedido de um pacote perdido a cada 105 ms, e ele acompanhava só os últimos 100 pacotes (cerca de 110 ms a 10 Mbps): um pedido perdido ou uma retransmissão perdida saía da janela antes da nova tentativa, e virava um pedido de keyframe e um congelamento. O nitra traz o str0m com essa janela aumentada para 1.000 pacotes ([`vendor/str0m`](vendor/str0m/NITRA.md)) e deixa o vídeo esperar as retransmissões ao longo de outros tantos. Os quadros que esperaram ainda chegam juntos, então 24–44% dos quadros ficam longos na tela com 2–5% de perda.

### Fluidez

Cada espectador escolhe quanto atraso trocar por fluidez (Configurações → "Ao assistir"); o padrão é automático. Os quadros são mostrados no seu horário de captura mais uma pequena espera, em vez de assim que chegam. 1080p60, 10 Mbps, 60 s por rodada:

| Condição | Modo | Quadros longos | Espaçamento p95 | Quadros mostrados | Espera (atraso a mais) |
| --- | --- | --- | --- | --- | --- |
| Sem problemas | Atraso mínimo | 10–17% | 31–34 ms | 3.672 | 0 |
| Sem problemas | Suave | 7–10% | 29–31 ms | 3.673 | 31 ms |
| Sem problemas | Automático | 9–13% | 31 ms | 3.653 | 10–16 ms |
| 2% de perda | Atraso mínimo | 24–29% | 47–48 ms | 3.413 | 0 |
| 2% de perda | Suave | 13–16% | 31 ms | 3.655 | 31 ms |
| 2% de perda | Automático | 12–15% | 31 ms | 3.648 | 28–31 ms |
| 5% de perda | Atraso mínimo | 40–42% | 50–60 ms | 3.166 | 0 |
| 5% de perda | Suave | 13–16% | 32–34 ms | 3.605 | 31 ms |
| 5% de perda | Automático | 15–19% | 31–36 ms | 3.544 | 31 ms |

A espera é de no máximo dois quadros (~33 ms a 60 fps); o automático a aumenta na hora para cobrir 95% das chegadas recentes e a diminui cerca de 2 ms por segundo. A irregularidade da própria janela de teste (veja acima) é o motivo de ele esperar 10–16 ms mesmo sem problemas de rede. A CPU de quem assiste foi a mesma em todos os modos (2,8–3,6 s de um núcleo por minuto, cerca de 5%); até três quadros esperam em texturas da GPU, cerca de 6 MB a mais de memória de vídeo por tela assistida em 1080p.

### Uma conexão mais lenta que a transmissão

| Conexão (transmissão de 10 Mbps) | Quadros mostrados em 60 s | Maior congelamento | Pedidos de keyframe | Mudanças no codificador |
| --- | --- | --- | --- | --- |
| 6 Mbps, codificador a 100% da estimativa | 1.699–2.201 | 1,9–3,3 s | 41–75 | 53–74 |
| **6 Mbps, a 85%, subindo em degraus** | **3.250** | **0,99 s** | **13** | 26 |
| 3 Mbps, codificador a 100% da estimativa | 1.606–1.917 | 3,1–3,2 s | 63–68 | 38–79 |
| **3 Mbps, a 85%, subindo em degraus** | **2.761** | **1,1 s** | **31** | 35 |

A 100% da estimativa de banda (o pacer envia cerca de 10% acima dela, e os keyframes vêm em rajadas), a fila do gargalo transbordava, os keyframes a transbordavam de novo e quem assistia continuava pedindo novos. Agora o codificador recebe 85% da estimativa, acompanha uma queda na hora e só sobe depois de 2 s, no máximo 15% por degrau: em 6 Mbps a imagem se mantém em 58–60 fps entre as quedas. Depois de uma queda grande, a volta leva cerca de meio minuto. Em conexões boas nada muda (10 Mbps, nenhuma mudança no codificador, nas mesmas rodadas).

## Segurança

- **Sem contas nem senhas.** Cada aparelho tem um par de chaves; a chave privada é protegida pelo DPAPI do Windows. As salas fazem a entrada por desafio e resposta com essa chave.
- **Mensagens assinadas**, com validade e proteção contra repetição. Convites manuais funcionam uma vez; convites de sala são guardados só como hashes, com validade e limite de usos.
- **Mídia cifrada**, ponto a ponto (DTLS-SRTP). As prévias são lacradas para cada membro da sala (X25519 e AES-256-GCM), e o servidor não consegue abri-las.
- **Sem endereços IP nos logs.** Os detalhes de conexão são registrados só como tipos e caminhos, e o Worker não guarda logs de requisições. O `scripts/check-logs.ps1` confere os logs atrás de endereços, convites, e-mails e segredos.
- **Protegido contra entradas malformadas:** testes de fuzz em todo leitor de dados de fora (protocolo das salas, convites, envelopes assinados, STUN, arquivos de perfil, prévias), de 1.000 a 50.000 entradas alteradas cada. As prévias só aceitam JPEG. Datagramas grandes demais são descartados em vez de encerrar a sessão. Os ids de transação do STUN vêm do gerador aleatório criptográfico do sistema.
- **Dependências auditadas** com o `cargo audit`: 147 dependências, 0 vulnerabilidades conhecidas.
- **Segurança da GPU:** só APIs documentadas de modo usuário. O nitra nunca mexe em clocks, voltagens, limites de energia, drivers ou nas configurações de TDR.

## Compilando

Requisitos: Windows 10 ou 11, o toolchain do Rust fixado em `rust-toolchain.toml` e a libopus e o FreeType estáticos do vcpkg (o runtime do C é ligado junto, veja `.cargo/config.toml`):

```powershell
vcpkg install opus:x64-windows-static freetype[core]:x64-windows-static
cargo build --release -p nitra
.\scripts\check.ps1
```

O `check.ps1` roda a formatação, o clippy e os testes. Uma compilação normal é de desenvolvimento, com a sua própria pasta de dados (`nitra-dev`) e o servidor de salas de desenvolvimento; definir `NITRA_CHANNEL=beta` na compilação gera a versão beta. O Worker das salas fica em `worker/` (TypeScript, Wrangler, Node.js 22 ou mais novo).

A fonte da interface é a [Inter](https://github.com/rsms/inter) e o logo usa a Bricolage Grotesque, as duas sob a SIL Open Font License; a Inter vai dentro do executável, a partir de `apps/nitra/assets/fonts` (licença no `OFL.txt` de lá).
