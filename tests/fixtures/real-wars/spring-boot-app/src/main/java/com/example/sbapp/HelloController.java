/*
 * Tomcat-RS — Spring Boot 3.x WAR fixture.
 *
 * A deliberately tiny controller that lets the integration test assert two
 * things at once:
 *
 *   * Spring MVC's DispatcherServlet was wired correctly by Spring Boot's
 *     SCI / WebApplicationInitializer (path → controller routing works),
 *   * The Tomcat-RS bridge can marshal a request's query string into the
 *     servlet (`@RequestParam` resolves), and the controller's JSON
 *     response makes it back out through the response facade.
 *
 * Endpoint: GET /hello?name=<n>  →  {"message": "Hello, <n>!"}
 * Default:  GET /hello           →  {"message": "Hello, World!"}
 */
package com.example.sbapp;

import java.util.Map;

import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RequestParam;
import org.springframework.web.bind.annotation.RestController;

@RestController
public class HelloController {

    @GetMapping("/hello")
    public Map<String, String> hello(
            @RequestParam(value = "name", defaultValue = "World") String name) {
        // Spring Boot's default Jackson HttpMessageConverter serialises this
        // Map as `{"message":"Hello, <name>!"}` with Content-Type
        // application/json.
        return Map.of("message", "Hello, " + name + "!");
    }
}
