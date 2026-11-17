cask "monkado" do
  version "1.0.0-nightly.20261014.58"
  sha256 "efbaf7adac3075c639249d387aa419bf44fe39ab84c079e4080cd6b0a228ed65"

  url "https://github.com/monkey-land/monkado/releases/download/v#{version}/monkado-v#{version}-macOS-ARM64.zip"
  name "Monkado"
  desc "Experimental native Discord client"
  homepage "https://github.com/monkey-land/monkado"

  # Frozen: this cask moved to the ViceVerse-cz/homebrew-monkado tap and is no longer updated here.
  deprecate! date: "2026-10-16",
             because: "moved to its own tap; run `brew untap ViceVerse-cz/monkado && brew tap ViceVerse-cz/monkado`"

  depends_on arch: :arm64
  depends_on macos: :sonoma

  app "Monkado.app"
end
