# Monkado

<p align="center">
  <img src="docs/monkado.png" alt="Monkado" width="160" />
</p>

<p align="center">
  <strong>Cliente de Discord nativo, leve e rápido, escrito em Rust — com compartilhamento de tela e câmera direto entre amigos.</strong>
</p>

<p align="center">
  <a href="https://github.com/monkey-land/Monkado/releases"><img src="https://img.shields.io/github/v/release/monkey-land/Monkado?label=vers%C3%A3o&color=blue" alt="Versão" /></a>
  <a href="LICENSE-MIT"><img src="https://img.shields.io/badge/licen%C3%A7a-MIT%20ou%20Apache--2.0-green.svg" alt="Licença" /></a>
  <img src="https://img.shields.io/badge/plataforma-Windows-informational.svg" alt="Windows" />
</p>

---

> [!WARNING]
> **Não oficial e sem nenhum vínculo com o Discord.**
> O Monkado conversa diretamente com o gateway e a API do Discord usando a sua própria conta. Usar clientes não oficiais vai contra os Termos de Serviço do Discord e pode levar ao banimento da conta. Use por sua conta e risco.

---

## 📦 Baixar e instalar

1. Vá em [**Releases**](https://github.com/monkey-land/Monkado/releases) e baixe o `monkado-<versão>-setup.exe`.
2. Rode o instalador. Ele instala só para o seu usuário em `%LOCALAPPDATA%\Programs\Monkado`, sem pedir administrador.
3. Abra o **Monkado** pelo Menu Iniciar e faça login com a sua conta do Discord (a tela de login é a oficial do Discord).

O Monkado se **atualiza sozinho**: quando sair uma versão nova aqui no GitHub, o app avisa e instala.

> Como o executável não é assinado, o Windows SmartScreen pode mostrar um aviso. Clique em **Mais informações → Executar assim mesmo**.

**Versão portátil:** baixe o `monkado-<versão>-Windows-X64.zip`, extraia em qualquer pasta e abra o `serein.exe`.

---

## 📺 Compartilhar tela e câmera (P2P)

O Discord bloqueia vídeo vindo de clientes não oficiais. Por isso o Monkado manda **tela e câmera direto de um computador para o outro**, sem passar pelos servidores do Discord:

- Entre na call normalmente (servidor ou DM). O **áudio continua pelo Discord**.
- Clique em compartilhar tela ou ligar a câmera, como sempre.
- Quem estiver na mesma call **usando o Monkado** vê o selo **AO VIVO** e a câmera ligada, e é só clicar para assistir.

Como funciona por baixo:
- A conexão é QUIC criptografada ([iroh](https://github.com/n0-computer/iroh)), atravessando roteador/NAT. Se a conexão direta não for possível, ela passa por um relay que só repassa dados criptografados.
- Só quem está na mesma call consegue se conectar: a chave de cada um é derivada da sessão de voz que o Discord entrega a todos da call.
- Ninguém assistindo = nada é enviado.

Limitações: só funciona entre pessoas usando o Monkado; quem está no Discord oficial não vê. Cada pessoa assistindo recebe a própria cópia do vídeo, então com muita gente o upload de quem compartilha pesa.

Para desligar o P2P (tela e câmera voltam a tentar pelo Discord), abra com a variável `SEREIN_P2P=0`.

---

## ⚡ Destaques

- **Nativo de verdade:** Rust + `egui` + `wgpu`. Sem Electron, sem navegador embutido. Usa muito menos memória e CPU que o app oficial.
- **Login seguro:** o token fica no Gerenciador de Credenciais do Windows. O login usa a página oficial do Discord numa janela temporária (WebView2), com suporte a QR code e 2FA.
- **Voz completa:** calls em DM, grupo e canais de servidor, push-to-talk, cancelamento de eco, supressão de ruído, Opus e criptografia ponta a ponta DAVE.
- **Fóruns e threads**, administração de servidor (cargos, permissões, logs de auditoria, convites, figurinhas), **GIFs e emojis**, envio de vários arquivos, editor de perfil, temas e extensões, player de vídeo, atalhos de teclado e detecção de jogos (Rich Presence).

---

## 🔒 Privacidade

- **Zero telemetria:** nada de analytics, rastreamento ou coleta de erros em segundo plano.
- O cache local (histórico recente, rascunhos, configurações) tem limite de tamanho e é apagado ao sair da conta.
- O registro de diagnóstico é opcional (Configurações → Atualizações) e só guarda textos fixos e números, nunca mensagens.

---

## 🛠️ Compilar a partir do código

Requisitos (Windows): Rust **1.99.1** (fixado em `rust-toolchain.toml`), Visual Studio Build Tools 2022 com C++, CMake, NASM, LLVM (libclang) e WebView2 Runtime.

```powershell
# Rodar com dados de demonstração (sem conta)
cargo run --locked --features demo -- --demo

# Rodar o app normal
cargo run --locked --release

# Gerar o pacote (dist/) e o instalador (precisa do NSIS instalado)
cargo xtask package
```

Documentação técnica (em inglês) fica em [`docs/`](docs/).

---

## 📄 Licença

O código é licenciado sob **MIT** ([LICENSE-MIT](LICENSE-MIT)) ou **Apache 2.0** ([LICENSE-APACHE](LICENSE-APACHE)), à sua escolha. Avisos de bibliotecas de terceiros, fontes e emojis estão em [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
