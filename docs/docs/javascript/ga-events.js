function GAizeTOCLink(l) {
  l.onclick = function () {
    url_test = l.className.match(/^language-ga-event-(.*)$/i);

    // console.log(url_test)

    toc_name = url_test[1];

    // console.log("Found TOC name ", toc_name)

    var that = this;
    // console.log("Sending GA event for toc link " + this.class + " with name " + toc_name);

    window.dataLayer = window.dataLayer || []

    function gtag() {
      dataLayer.push(arguments)
    }

    gtag('event', 'docs_click', {'event_category': 'docs_code', 'event_label': toc_name});

  };

}

document.addEventListener("DOMContentLoaded", function () {

  var toc_links = document.querySelectorAll('code');
  for (i = 0; i < toc_links.length; i++) {
    if (toc_links[i].className.match(/^language-ga-event-(.*)/i)) {
      // console.log("Found TOC link ", toc_links[i].className)
      GAizeTOCLink(toc_links[i]);
    }
  }

});
