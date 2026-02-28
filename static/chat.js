(function () {
    const messages = document.getElementById("messages");
    const form = document.getElementById("chat-form");
    const input = document.getElementById("input");
    const sendBtn = document.getElementById("send-btn");
    const statusIndicator = document.getElementById("status-indicator");
    const entityName = document.getElementById("entity-name");

    let sending = false;

    // Check server status on load
    checkStatus();

    form.addEventListener("submit", async (e) => {
        e.preventDefault();
        const text = input.value.trim();
        if (!text || sending) return;

        addMessage(text, "user");
        input.value = "";
        setSending(true);

        const typing = addTyping();

        try {
            const res = await fetch("/chat", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ message: text, channel: "chat" }),
            });

            typing.remove();

            if (!res.ok) {
                const errText = await res.text();
                addMessage("Error: " + errText, "error");
                return;
            }

            const data = await res.json();
            const meta = [];
            if (data.model) meta.push(data.model);
            if (data.input_tokens) meta.push(data.input_tokens + " in");
            if (data.output_tokens) meta.push(data.output_tokens + " out");

            addMessage(data.response, "assistant", meta.join(" · "));
        } catch (err) {
            typing.remove();
            addMessage("Connection error: " + err.message, "error");
        } finally {
            setSending(false);
            input.focus();
        }
    });

    function addMessage(text, role, meta) {
        const div = document.createElement("div");
        div.className = "message " + role;
        div.textContent = text;

        if (meta) {
            const metaEl = document.createElement("div");
            metaEl.className = "meta";
            metaEl.textContent = meta;
            div.appendChild(metaEl);
        }

        messages.appendChild(div);
        messages.scrollTop = messages.scrollHeight;
        return div;
    }

    function addTyping() {
        const div = document.createElement("div");
        div.className = "typing";
        div.textContent = "Thinking";
        messages.appendChild(div);
        messages.scrollTop = messages.scrollHeight;
        return div;
    }

    function setSending(state) {
        sending = state;
        input.disabled = state;
        sendBtn.disabled = state;
    }

    async function checkStatus() {
        try {
            const res = await fetch("/api/status");
            if (res.ok) {
                const data = await res.json();
                statusIndicator.textContent = "online";
                statusIndicator.className = "status online";
                if (data.entity) {
                    entityName.textContent = data.entity;
                    document.title = data.entity;
                }
            }
        } catch {
            statusIndicator.textContent = "offline";
            statusIndicator.className = "status offline";
        }
    }
})();
