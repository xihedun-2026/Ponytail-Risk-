const form = document.querySelector("#home-login");
const keyInput = document.querySelector("#home-key");
const errorBox = document.querySelector("#home-error");

form.addEventListener("submit", async (event) => {
  event.preventDefault();
  errorBox.textContent = "";
  const response = await fetch("/api/login", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ key: keyInput.value }),
  });
  if (response.ok) {
    location.href = "/app";
    return;
  }
  const result = await response.json().catch(() => ({}));
  errorBox.textContent = result.error || "登录失败，请稍后重试";
});

document.querySelectorAll(".stage-button").forEach((button) => {
  button.addEventListener("click", () => {
    document.querySelectorAll(".stage-button").forEach((item) => item.classList.remove("active"));
    button.classList.add("active");
    document.querySelector(".capability-grid").dataset.stage = button.dataset.stage;
  });
});
