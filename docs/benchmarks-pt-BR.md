# nitra: benchmarks e medições

**Leia em:** [English](benchmarks.md) · Português (Brasil) · [Voltar ao README](../README-pt-BR.md)

## Benchmarks

Medido num Ryzen 7 5800XT (16 threads) com uma RX 7600, um monitor principal de 320 Hz e um segundo monitor de 144 Hz. Os resultados no jogo vêm do Campo de Treino do Valorant, em rodadas repetidas e alternadas, comparadas com intervalos de confiança de 95%; "sem diferença mensurável" quer dizer que o intervalo inclui o zero. A CPU é uma porcentagem de um núcleo, com a parcela do processador inteiro entre parênteses quando importa. A metodologia está em [`docs/benchmark-protocol.md`](benchmark-protocol.md) (em inglês).

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

Um espectador que só recebe não tem tempo de ida e volta pelo RTP, então o str0m repete o pedido de um pacote perdido a cada 105 ms, e ele acompanhava só os últimos 100 pacotes (cerca de 110 ms a 10 Mbps): um pedido perdido ou uma retransmissão perdida saía da janela antes da nova tentativa, e virava um pedido de keyframe e um congelamento. O nitra traz o str0m com essa janela aumentada para 1.000 pacotes ([`vendor/str0m`](../vendor/str0m/NITRA.md)) e deixa o vídeo esperar as retransmissões ao longo de outros tantos. Os quadros que esperaram ainda chegam juntos, então 24–44% dos quadros ficam longos na tela com 2–5% de perda.

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
