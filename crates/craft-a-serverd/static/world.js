/* Uploads a world zip from the World page. Streams the chosen file as the
   request body (the browser reads it from disk, not into memory) and shows
   progress. No inline code anywhere: the Content-Security-Policy forbids it. */
(function () {
  "use strict";

  var form = document.getElementById("world-upload");
  if (!form) {
    return;
  }
  var input = document.getElementById("world-file");
  var bar = document.getElementById("world-progress");
  var message = document.getElementById("world-message");
  var button = form.querySelector("button[type=submit]");
  var max = parseInt(form.getAttribute("data-max"), 10) || 0;

  function say(text) {
    message.textContent = text;
  }

  function size(bytes) {
    if (bytes >= 1073741824) {
      return (bytes / 1073741824).toFixed(1) + " GiB";
    }
    return Math.max(1, Math.round(bytes / 1048576)) + " MiB";
  }

  function fail(text) {
    button.disabled = false;
    input.disabled = false;
    bar.hidden = true;
    say(text);
  }

  form.addEventListener("submit", function (event) {
    event.preventDefault();
    var file = input.files && input.files[0];
    if (!file) {
      say("Choose a .zip file first.");
      return;
    }
    if (!/\.zip$/i.test(file.name)) {
      say("The file must be a .zip.");
      return;
    }
    if (max && file.size > max) {
      say("That file is too big for the free space here (limit " + size(max) + ").");
      return;
    }

    var request = new XMLHttpRequest();
    request.open("POST", form.getAttribute("action"));
    request.setRequestHeader("X-CSRF-Token", form.getAttribute("data-csrf"));
    request.setRequestHeader("Accept", "application/json");
    request.setRequestHeader("Content-Type", "application/zip");
    request.upload.onprogress = function (e) {
      if (e.lengthComputable) {
        bar.value = Math.round((e.loaded * 100) / e.total);
        say("Uploading... " + bar.value + "% of " + size(e.total));
      }
    };
    request.onload = function () {
      if (request.status >= 200 && request.status < 300) {
        say("Upload finished. Opening the review...");
        window.location.assign("/world/import");
        return;
      }
      var text = "The upload failed (error " + request.status + ").";
      try {
        var body = JSON.parse(request.responseText);
        if (body && body.error && body.error.message) {
          text = body.error.message;
        }
      } catch (ignored) {
        /* keep the generic text */
      }
      fail(text);
    };
    request.onerror = function () {
      fail("The connection was lost during the upload.");
    };

    button.disabled = true;
    input.disabled = true;
    bar.value = 0;
    bar.hidden = false;
    say("Uploading...");
    request.send(file);
  });
})();
