<!-- Author: Nicola Antonello -->

# Benchmarking

Measuring latency and throughput without fooling yourself. Part of the [reading list](README.md).

- ★ Gil Tene, [How NOT to Measure Latency](https://www.infoq.com/presentations/latency-response-time/) (QCon San Francisco, 2015). Coordinated omission, and why percentiles cannot be averaged.
- Gernot Heiser, [Systems Benchmarking Crimes](https://gernot-heiser.org/benchmarking-crimes.html). A checklist of misleading benchmarking practices to avoid.
- Brendan Gregg, [Thinking Methodically about Performance](https://doi.org/10.1145/2405116.2413037) (ACM Queue, 2012). The USE method: check utilization, saturation and errors for every resource.
- Jeffrey Dean and Luiz André Barroso, [The Tail at Scale](https://doi.org/10.1145/2408776.2408794) (Communications of the ACM, 2013). Why tail latency matters at scale, and techniques that tolerate latency variability.
- Bianca Schroeder, Adam Wierman and Mor Harchol-Balter, [Open Versus Closed: A Cautionary Tale](https://www.usenix.org/conference/nsdi-06/open-versus-closed-cautionary-tale) (NSDI, 2006). Closed-loop and open-loop load generators report different response times at the same load.
