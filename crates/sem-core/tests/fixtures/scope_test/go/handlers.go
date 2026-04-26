package handlers

import (
	"myapp/models"
	"myapp/service"
)

func HandleCreateDog(name string) *models.Dog {
	return service.CreateDog(name)
}

func HandleCreateCat(name string) *models.Cat {
	return service.CreateCat(name)
}

func HandleTransfer(name string) int {
	shelter := models.NewShelter()
	dog := models.NewDog(name)
	service.TransferAnimal(dog.Name, shelter)
	return shelter.Count()
}

func HandleList() []string {
	animals := service.ListAnimals()
	return animals
}

func Validate(name string) bool {
	return len(name) > 0
}
